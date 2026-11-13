//! MHTML (MIME HTML, RFC 2557): writing documents as MHTML archives and reading them back.
//!
//! An MHTML archive is a multipart/related MIME message: the first (or "start") part is the document,
//! every other part is one of its assets, named by Content-Location (its URL) and/or Content-ID (cid: URL).
//! Archives saved by Chromium-based browsers, Internet Explorer, Word, Firefox add-ons and monolith itself
//! all look like that, they just differ in which headers and transfer encodings they use.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};

use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, PAD_INDIFFERENT};
use base64::{Engine, prelude::BASE64_STANDARD};
use chrono::Utc;
use url::Url;

use crate::core::{detect_media_type, is_plaintext_media_type};
use crate::session::Session;
use crate::source_maps::remove_source_map_reference;
use crate::url::clean_url;

/// Boundary between parts of MHTML documents monolith writes.
/// Bodies get encoded in a way that never has a line start with "-", so it can't collide with anything.
pub const MHTML_BOUNDARY: &str = "----MultipartBoundary--monolith----";

// Max length of encoded lines, as per RFC 2045 (not counting CRLF)
const MAX_LINE_LENGTH: usize = 76;

// Base64 encoders out there aren't always strict about padding and trailing bits, readers shouldn't be either
const BASE64_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    PAD_INDIFFERENT.with_decode_allow_trailing_bits(true),
);

/// MIME headers (names and values), in order of appearance
pub type Headers = Vec<(String, String)>;

/// A single part of an MHTML archive, already decoded.
#[derive(Debug)]
pub struct ArchivedAsset {
    pub data: Vec<u8>,
    /// Content-Location of the part, resolved into a full URL (None if the part doesn't have one)
    pub url: Option<Url>,
    pub media_type: String,
    /// Empty if not specified
    pub charset: String,
    /// What it was received with, for archives that keep that (captures of pages, WARC, HAR):
    /// it says what other origins have allowed pages to do with it (see cors.rs)
    pub response_headers: Option<Vec<(String, String)>>,
}

#[derive(Debug)]
pub struct MhtmlArchive {
    assets: Vec<ArchivedAsset>,
    /// Lookup table: URL (without fragment) or "cid:<Content-ID>" -> index in assets
    index: HashMap<String, usize>,
    root: usize,
    /// Whether parts of nested MHTML archives got unpacked into this one (see parse_nested_mhtml)
    nested: bool,
    /// Scope (multipart node ID) each asset came from, by asset index; empty for flat archives
    asset_scopes: Vec<usize>,
    /// Scope each URL index entry belongs to, by its key; absent for flat archives
    entry_scopes: HashMap<String, usize>,
    /// Parent scope of every scope, by scope ID; the root scope's parent is None
    scope_parents: HashMap<usize, usize>,
}

/// Errors nested unpacking can run into (RFC 2557 structure problems that make the result ambiguous).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MhtmlError(pub String);

impl fmt::Display for MhtmlError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for MhtmlError {}

fn mhtml_error(message: impl Into<String>) -> MhtmlError {
    MhtmlError(message.into())
}

impl MhtmlArchive {
    /// Creates an archive out of assets and the URLs they're known by, the first one is the document.
    pub fn from_assets(assets: Vec<(Vec<Url>, ArchivedAsset)>) -> MhtmlArchive {
        let mut archive = MhtmlArchive {
            assets: Vec::with_capacity(assets.len()),
            index: HashMap::new(),
            root: 0,
            nested: false,
            asset_scopes: Vec::new(),
            entry_scopes: HashMap::new(),
            scope_parents: HashMap::new(),
        };

        for (i, (urls, asset)) in assets.into_iter().enumerate() {
            for url in urls {
                archive.index.entry(archive_key(&url)).or_insert(i);
            }
            archive.assets.push(asset);
        }

        archive
    }

    /// The document itself.
    pub fn root(&self) -> &ArchivedAsset {
        &self.assets[self.root]
    }

    /// Scope the document itself belongs to.
    pub fn root_scope(&self) -> Option<usize> {
        self.asset_scopes.get(self.root).copied()
    }

    /// Whether parts of nested MHTML archives got unpacked into this one.
    pub fn is_nested(&self) -> bool {
        self.nested
    }

    pub fn get(&self, url: &Url) -> Option<&ArchivedAsset> {
        self.index.get(&archive_key(url)).map(|i| &self.assets[*i])
    }

    /// Same as get(), but for Content-ID references it only looks in the scope the referencing
    /// part belongs to and in the scopes around it (never in sibling packages, see parse_nested_mhtml).
    pub fn get_for(&self, parent_url: &Url, url: &Url) -> Option<&ArchivedAsset> {
        let key: String = archive_key(url);
        if !url.scheme().eq_ignore_ascii_case("cid") || self.asset_scopes.is_empty() {
            return self.index.get(&key).map(|i| &self.assets[*i]);
        }

        // What's making the request says which scope it belongs to
        let parent_key: String = archive_key(parent_url);
        let mut scope: Option<usize> = self
            .entry_scopes
            .get(&parent_key)
            .copied()
            .or_else(|| {
                self.index
                    .get(&parent_key)
                    .and_then(|i| self.asset_scopes.get(*i))
                    .copied()
            })
            // The document itself (or a part without a recorded location) asks from the root scope
            .or_else(|| self.asset_scopes.get(self.root).copied());

        // Walk up the containers: the package it belongs to first, then every ancestor of it.
        // Sibling packages never answer, and when the requesting part's scope is known there's no
        // global fallback: a CID that none of its containers define is simply not there.
        while let Some(current) = scope {
            if let Some(&i) = self.index.get(&scoped_cid_key(&key, current)) {
                return Some(&self.assets[i]);
            }
            scope = self.scope_parents.get(&current).copied();
        }

        // Only requests without a known scope (parts outside the scoped tree) use the flat lookup
        let parent_known = self
            .entry_scopes
            .contains_key(&parent_key)
            || self.index.contains_key(&parent_key);
        if parent_known {
            None
        } else {
            self.index.get(&key).map(|i| &self.assets[*i])
        }
    }

    /// Scope the asset known by given URL belongs to, if any.
    pub fn scope_of(&self, url: &Url) -> Option<usize> {
        let key: String = archive_key(url);
        self.entry_scopes
            .get(&key)
            .copied()
            .or_else(|| self.index.get(&key).and_then(|i| self.asset_scopes.get(*i)).copied())
    }

    pub fn contains(&self, url: &Url) -> bool {
        self.index.contains_key(&archive_key(url))
    }

    /// Number of parts in the archive (the document included).
    pub fn len(&self) -> usize {
        self.assets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }
}

/// Key under which a Content-ID gets indexed within its specific scope (synthetic, never a real URL).
fn scoped_cid_key(cid_key: &str, scope: usize) -> String {
    // cid_key is "cid:<id>", the scope suffix makes same-named IDs in different packages distinct
    format!("{cid_key}@{scope}")
}

fn archive_key(url: &Url) -> String {
    if url.scheme().eq_ignore_ascii_case("cid") {
        // RFC 2392: cid: URLs are percent-encoded Content-IDs
        format!("cid:{}", percent_decode(url.path()))
    } else {
        clean_url(url.clone()).as_str().to_string()
    }
}

/// Checks if the data looks like an MHTML archive (a MIME message with multipart/related or multipart/mixed content).
pub fn is_mhtml(data: &[u8]) -> bool {
    match parse_headers(data, true) {
        Some((headers, _)) => multipart_boundary(&headers).is_some(),
        None => false,
    }
}

/// Unpacks an MHTML archive.
/// Relative Content-Location values get resolved against the archive's own location (if it has one), or fallback_url.
/// Returns None if the data isn't an MHTML archive, or if there's nothing in it.
///
/// This is the flat, original mode: every part of the top-level message becomes an asset, parts that
/// are MHTML archives themselves stay packed. See parse_nested_mhtml() for the nested mode.
pub fn parse_mhtml(data: &[u8], fallback_url: Option<&Url>) -> Option<MhtmlArchive> {
    let (parts, start) = split_parts(data, fallback_url)?;

    let mut archive = MhtmlArchive {
        assets: Vec::with_capacity(parts.len()),
        index: HashMap::new(),
        root: 0,
        nested: false,
        asset_scopes: Vec::new(),
        entry_scopes: HashMap::new(),
        scope_parents: HashMap::new(),
    };
    let mut root: Option<usize> = None;

    for part in parts {
        let i: usize = archive.assets.len();
        index_part(&mut archive.index, &part, i, &mut root, start.as_deref());
        archive.assets.push(part.into_asset());
    }

    if archive.assets.is_empty() {
        return None;
    }

    // Without the "start" parameter it's the first part, unless that one isn't a document (and some other part is)
    archive.root = root.unwrap_or_else(|| {
        archive
            .assets
            .iter()
            .position(|asset| is_html_media_type(&asset.media_type))
            .unwrap_or(0)
    });

    Some(archive)
}

/// Same as parse_mhtml(), but unpacking nested MHTML archives recursively: parts that are themselves
/// MHTML messages (multipart/related, multipart/mixed, multipart/alternative, or message/rfc822) get
/// unpacked, and the document to process gets chosen out of the whole tree.
///
/// Root selection, per multipart node:
/// - multipart/related: the part named by the "start" parameter (its direct child), or the first child
/// - multipart/mixed: the first child branch that leads to an HTML document
/// - multipart/alternative: the last child (the richest alternative)
///
/// Content-ID references get resolved in the package the referencing part belongs to first, then in
/// every package around it (ancestors); sibling packages never answer. Same-named Content-IDs in the
/// same package are ambiguous and rejected, same-named IDs in different packages both stay usable.
///
/// Relative Content-Location values get resolved against the container part's own location, then
/// against each surrounding container's, and finally against fallback_url. Nothing is fetched:
/// a referenced part that isn't in the archive is an error reported by the caller while walking.
pub fn parse_nested_mhtml(
    data: &[u8],
    fallback_url: Option<&Url>,
) -> Result<MhtmlArchive, MhtmlError> {
    let (headers, body) =
        parse_headers(data, true).ok_or_else(|| mhtml_error("not an MHTML archive"))?;
    let (boundary, _) =
        multipart_boundary(&headers).ok_or_else(|| mhtml_error("not an MHTML archive"))?;
    let base_url = message_base_url(&headers, fallback_url);

    let mut builder = TreeBuilder {
        next_scope: 0,
        scope_parents: HashMap::new(),
    };
    let (media_type, params) =
        parse_content_type(get_header(&headers, "content-type").unwrap_or_default());
    let start: Option<String> = start_of(&params);
    let root_node: Node = builder.parse_multipart(
        body,
        &boundary,
        None,
        base_url.as_ref(),
        media_type,
        start,
    )?;
    let root_part: &Part = root_node
        .selected()?
        .ok_or_else(|| mhtml_error("MHTML archive has no HTML document in it"))?;
    if !is_html_media_type(&root_part.asset.media_type) {
        return Err(mhtml_error(format!(
            "selected part is not an HTML document ({}), nothing to save",
            root_part.asset.media_type
        )));
    }

    let mut archive = MhtmlArchive {
        assets: Vec::new(),
        index: HashMap::new(),
        root: 0,
        nested: true,
        asset_scopes: Vec::new(),
        entry_scopes: HashMap::new(),
        scope_parents: builder.scope_parents,
    };

    // Every leaf of the tree becomes an asset; the selected root is remembered by its part
    flatten_node(
        &root_node,
        &mut archive,
        &mut HashMap::new(),
        Some(root_part),
    )?;

    if archive.assets.is_empty() {
        return Err(mhtml_error("MHTML archive has nothing in it"));
    }

    Ok(archive)
}

/// A part of a multipart message, headers parsed and body decoded.
struct RawPart {
    data: Vec<u8>,
    media_type: String,
    charset: String,
    url: Option<Url>,
    content_id: Option<String>,
}

impl RawPart {
    fn into_asset(self) -> ArchivedAsset {
        ArchivedAsset {
            data: self.data,
            url: self.url,
            media_type: self.media_type,
            charset: self.charset,
            response_headers: None,
        }
    }
}

/// A leaf part, with the scope (multipart node ID) it belongs to.
struct Part {
    scope: usize,
    asset: ArchivedAsset,
    content_id: Option<String>,
}

/// A node of the parsed MIME tree: either a leaf part or a multipart container with its children.
enum Node {
    Leaf(Part),
    Multipart {
        scope: usize,
        media_type: String,
        /// Content-ID named by the "start" parameter, if the container had one
        start: Option<String>,
        children: Vec<Node>,
    },
}

impl Node {
    /// The leaf that documents inside this node should be taken from.
    /// An invalid "start" parameter (no direct child with that Content-ID) is an error.
    fn selected(&self) -> Result<Option<&Part>, MhtmlError> {
        match self {
            Node::Leaf(part) => Ok(Some(part)),
            Node::Multipart {
                media_type,
                start,
                children,
                scope,
            } => {
                if media_type == "multipart/related" {
                    let Some(child) = (if let Some(start) = start {
                        // The part "start" names has to be a direct child of this container
                        let found = children
                            .iter()
                            .find(|child| child.direct_content_id() == Some(start.as_str()));
                        if found.is_none() {
                            return Err(mhtml_error(format!(
                                "multipart/related node (scope {scope}) has no part with Content-ID \"{start}\" named by its start parameter"
                            )));
                        }
                        found
                    } else {
                        children.first()
                    }) else {
                        return Ok(None);
                    };
                    child.selected()
                } else if media_type == "multipart/alternative" {
                    // The last alternative is the richest one
                    match children.last() {
                        Some(child) => child.selected(),
                        None => Ok(None),
                    }
                } else {
                    // multipart/mixed: the first branch that leads to an HTML document
                    for child in children {
                        if let Some(part) = child.selected()?
                            && is_html_media_type(&part.asset.media_type)
                        {
                            return Ok(Some(part));
                        }
                    }
                    Ok(None)
                }
            }
        }
    }

    /// Content-ID of this node when it's a leaf part, None otherwise (for direct-child "start" matching).
    fn direct_content_id(&self) -> Option<&str> {
        match self {
            Node::Leaf(part) => part.content_id.as_deref(),
            Node::Multipart { .. } => None,
        }
    }
}

struct TreeBuilder {
    next_scope: usize,
    scope_parents: HashMap<usize, usize>,
}

impl TreeBuilder {
    fn new_scope(&mut self, parent: Option<usize>) -> usize {
        let scope: usize = self.next_scope;
        self.next_scope += 1;
        if let Some(parent) = parent {
            self.scope_parents.insert(scope, parent);
        }
        scope
    }

    /// Parses a multipart body into a tree of nodes.
    fn parse_multipart(
        &mut self,
        body: &[u8],
        boundary: &str,
        parent_scope: Option<usize>,
        parent_base_url: Option<&Url>,
        media_type: String,
        start: Option<String>,
    ) -> Result<Node, MhtmlError> {
        let scope: usize = self.new_scope(parent_scope);
        let mut children: Vec<Node> = Vec::new();
        // Content-IDs of direct leaf children: duplicates within one package are ambiguous
        let mut content_ids: HashMap<String, ()> = HashMap::new();

        for raw in split_multipart(body, boundary) {
            let (headers, part_body) = parse_headers(raw, false).unwrap_or_default();
            let (child_media_type, params) =
                parse_content_type(get_header(&headers, "content-type").unwrap_or_default());
            let part_base_url = part_base_url(&headers, parent_base_url);

            if let Some((child_boundary, _)) = multipart_node_boundary(&headers) {
                // A nested multipart message (related, mixed, or alternative)
                let node = self.parse_multipart(
                    part_body,
                    &child_boundary,
                    Some(scope),
                    part_base_url.as_ref(),
                    child_media_type,
                    start_of(&params),
                )?;
                children.push(node);
                continue;
            }

            if child_media_type == "message/rfc822" {
                // A whole MIME message packed as a part: its own headers and body follow
                let node = self
                    .parse_message(part_body, Some(scope), part_base_url.as_ref())?
                    .ok_or_else(|| mhtml_error("empty message/rfc822 part"))?;
                children.push(node);
                continue;
            }

            let part = decode_part(headers, part_body, part_base_url.as_ref());

            // A leaf part that is itself an MHTML archive: either declared so, or an unnamed part
            // whose body is a MIME message (nested messages often have no part-level Content-Type)
            let looks_like_message = is_mhtml(&part.data)
                || parse_headers(&part.data, true)
                    .and_then(|(headers, _)| multipart_node_boundary(&headers))
                    .is_some();
            if let Some(node) = looks_like_message
                .then(|| {
                    self.parse_message(
                        &part.data,
                        Some(scope),
                        part.url.as_ref().or(part_base_url.as_ref()),
                    )
                })
                .transpose()?
                .flatten()
            {
                children.push(node);
                continue;
            }

            if let Some(content_id) = &part.content_id
                && content_ids.insert(content_id.clone(), ()).is_some()
            {
                return Err(mhtml_error(format!(
                    "duplicate Content-ID \"{content_id}\" in the same MHTML package"
                )));
            }

            let content_id = part.content_id.clone();
            children.push(Node::Leaf(Part {
                scope,
                asset: part.into_asset(),
                content_id,
            }));
        }

        if children.is_empty() {
            return Err(mhtml_error("MHTML multipart node has no parts"));
        }

        Ok(Node::Multipart {
            scope,
            media_type,
            start,
            children,
        })
    }

    /// Parses a standalone MIME message (headers + multipart body), as found in message/rfc822 parts.
    fn parse_message(
        &mut self,
        data: &[u8],
        parent_scope: Option<usize>,
        parent_base_url: Option<&Url>,
    ) -> Result<Option<Node>, MhtmlError> {
        // A nested message may be preceded by blank lines (part padding); skip them
        let data = data
            .iter()
            .position(|&b| b != b'\r' && b != b'\n')
            .map(|start| &data[start..])
            .unwrap_or(data);
        let Some((headers, body)) = parse_headers(data, false) else {
            return Ok(None);
        };
        let Some((boundary, _)) = multipart_node_boundary(&headers) else {
            return Ok(None);
        };
        let (media_type, params) =
            parse_content_type(get_header(&headers, "content-type").unwrap_or_default());
        let base_url = message_base_url(&headers, parent_base_url.cloned().as_ref());
        Ok(Some(self.parse_multipart(
            body,
            &boundary,
            parent_scope,
            base_url.as_ref(),
            media_type,
            start_of(&params),
        )?))
    }
}

/// Flattens the tree into archive assets, remembering each part's scope and building the lookup index.
fn flatten_node<'a>(
    node: &'a Node,
    archive: &mut MhtmlArchive,
    content_ids_by_scope: &mut HashMap<(usize, String), ()>,
    selected: Option<&'a Part>,
) -> Result<(), MhtmlError> {
    match node {
        Node::Leaf(part) => {
            let index: usize = archive.assets.len();
            if selected.is_some_and(|selected| std::ptr::eq(selected, part)) {
                archive.root = index;
            }
            archive.asset_scopes.push(part.scope);

            if let Some(url) = &part.asset.url
                && url.scheme() != "data"
            {
                let key = archive_key(url);
                archive.index.entry(key.clone()).or_insert(index);
                archive.entry_scopes.entry(key).or_insert(part.scope);
            }
            if let Some(content_id) = &part.content_id {
                let cid_key: String = format!("cid:{content_id}");
                // Same Content-ID in the same package was already rejected while building the tree;
                // the guard also covers IDs repeated across flattened branches of one scope
                if content_ids_by_scope
                    .insert((part.scope, content_id.clone()), ())
                    .is_some()
                    && !archive
                        .index
                        .contains_key(&scoped_cid_key(&cid_key, part.scope))
                {
                    return Err(mhtml_error(format!(
                        "duplicate Content-ID \"{content_id}\" in the same MHTML package"
                    )));
                }
                // Scoped entry: same-named IDs in inner and outer packages both stay usable
                archive
                    .index
                    .entry(scoped_cid_key(&cid_key, part.scope))
                    .or_insert(index);
                archive
                    .entry_scopes
                    .entry(scoped_cid_key(&cid_key, part.scope))
                    .or_insert(part.scope);
                // The unscoped entry keeps the closest (deepest) part for flat lookups
                archive.index.entry(cid_key.clone()).or_insert(index);
                archive.entry_scopes.entry(cid_key).or_insert(part.scope);
            }

            archive.assets.push(clone_asset(&part.asset));
            Ok(())
        }
        Node::Multipart { children, .. } => {
            for child in children {
                // Only the branch the selection points at can contain the selected part
                let child_selected = selected.filter(|selected| node_contains(child, selected));
                flatten_node(child, archive, content_ids_by_scope, child_selected)?;
            }
            Ok(())
        }
    }
}

/// Whether given node is the selected part itself or a container around it.
fn node_contains<'a>(node: &'a Node, selected: &'a Part) -> bool {
    match node {
        Node::Leaf(part) => std::ptr::eq(part, selected),
        Node::Multipart { children, .. } => {
            children.iter().any(|child| node_contains(child, selected))
        }
    }
}

fn clone_asset(asset: &ArchivedAsset) -> ArchivedAsset {
    ArchivedAsset {
        data: asset.data.clone(),
        url: asset.url.clone(),
        media_type: asset.media_type.clone(),
        charset: asset.charset.clone(),
        response_headers: asset.response_headers.clone(),
    }
}

/// Splits the top-level message into its decoded parts (flat mode), with the "start" Content-ID.
fn split_parts(data: &[u8], fallback_url: Option<&Url>) -> Option<(Vec<RawPart>, Option<String>)> {
    let (headers, body) = parse_headers(data, true)?;
    let (boundary, start) = multipart_boundary(&headers)?;
    let base_url = message_base_url(&headers, fallback_url);

    let parts: Vec<RawPart> = split_multipart(body, &boundary)
        .into_iter()
        .map(|raw| {
            let (part_headers, part_body) = parse_headers(raw, false).unwrap_or_default();
            let part_base_url = part_base_url(&part_headers, base_url.as_ref());
            decode_part(part_headers, part_body, part_base_url.as_ref())
        })
        .collect();

    (!parts.is_empty()).then_some((parts, start))
}

/// Decodes one part: transfer encoding, media type, charset, Content-Location and Content-ID.
fn decode_part(headers: Headers, body: &[u8], base_url: Option<&Url>) -> RawPart {
    let (mut media_type, params) =
        parse_content_type(get_header(&headers, "content-type").unwrap_or_default());
    let charset: String = params.get("charset").cloned().unwrap_or_default();

    let data: Vec<u8> = match get_header(&headers, "content-transfer-encoding")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "base64" => decode_base64(body),
        "quoted-printable" => decode_quoted_printable(body),
        _ => body.to_vec(), // 7bit, 8bit, binary
    };

    let url: Option<Url> = get_header(&headers, "content-location").and_then(|location| {
        let location = location.trim();
        Url::parse(location).ok().or_else(|| {
            base_url
                .as_ref()
                .and_then(|base_url| base_url.join(location).ok())
        })
    });

    let content_id: Option<String> = get_header(&headers, "content-id")
        .map(|id| {
            id.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
        .filter(|id| !id.is_empty());

    if media_type.is_empty() {
        media_type = match &url {
            Some(url) => detect_media_type(&data, url),
            None => detect_media_type(&data, &Url::parse("data:,").unwrap()),
        };
    }

    RawPart {
        data,
        media_type,
        charset,
        url,
        content_id,
    }
}

/// Adds a flat-mode part to the lookup index and tracks the "start"-named root.
fn index_part(
    index: &mut HashMap<String, usize>,
    part: &RawPart,
    i: usize,
    root: &mut Option<usize>,
    start: Option<&str>,
) {
    // Parts that come first win, in case there are duplicates
    if let Some(url) = &part.url
        && url.scheme() != "data"
    {
        index.entry(archive_key(url)).or_insert(i);
    }
    if let Some(content_id) = &part.content_id {
        index
            .entry(format!("cid:{}", content_id))
            .or_insert(i);

        if root.is_none() && start == Some(content_id.as_str()) {
            *root = Some(i);
        }
    }
}

/// Base URL declared by a message itself (Content-Location/Content-Base/Snapshot-Content-Location).
fn message_base_url(headers: &Headers, fallback_url: Option<&Url>) -> Option<Url> {
    [
        "content-location",
        "content-base",
        "snapshot-content-location",
    ]
    .iter()
    .filter_map(|name| get_header(headers, name))
    .find_map(|value| Url::parse(value.trim()).ok())
    .or_else(|| fallback_url.cloned())
}

/// Base URL for parts of a message: the part's own absolute Content-Location wins, otherwise
/// it inherits the container's base URL, so relative locations resolve layer by layer.
fn part_base_url(headers: &Headers, parent_base_url: Option<&Url>) -> Option<Url> {
    get_header(headers, "content-location")
        .and_then(|location| Url::parse(location.trim()).ok())
        .or_else(|| parent_base_url.cloned())
}

/// Packs the document and all assets it references into an MHTML archive.
/// The document's assets must already have been logged by the session (see Session::log_asset_url).
pub fn create_mhtml_document(
    session: &mut Session,
    html: Vec<u8>,
    document_url: &Url,
    document_location: Option<&Url>,
    document_charset: &str,
    document_title: Option<&str>,
) -> Vec<u8> {
    let mut mhtml: Vec<u8> = Vec::with_capacity(html.len() * 2);

    write_mhtml_document(
        session,
        &mut mhtml,
        html,
        document_url,
        document_location,
        document_charset,
        document_title,
    )
    .expect("Unable to write MHTML into buffer");

    mhtml
}

/// Writes the archive out part by part, assets get retrieved as they're needed.
pub(crate) fn write_mhtml_document(
    session: &mut Session,
    out: &mut dyn Write,
    html: Vec<u8>,
    document_url: &Url,
    document_location: Option<&Url>,
    document_charset: &str,
    document_title: Option<&str>,
) -> io::Result<()> {
    let mut mhtml: Vec<u8> = vec![];

    // Message headers
    mhtml.extend_from_slice(b"From: <Saved by Monolith>\r\n");
    if let Some(location) = document_location {
        push_header(&mut mhtml, "Snapshot-Content-Location", location.as_str());
    }
    if let Some(title) = document_title.filter(|title| !title.trim().is_empty()) {
        push_header(&mut mhtml, "Subject", &encode_header_value(title));
    }
    push_header(&mut mhtml, "Date", &Utc::now().to_rfc2822());
    mhtml.extend_from_slice(b"MIME-Version: 1.0\r\n");
    mhtml.extend_from_slice(
        format!(
            "Content-Type: multipart/related;\r\n\ttype=\"text/html\";\r\n\tboundary=\"{}\"\r\n\r\n",
            MHTML_BOUNDARY
        )
        .as_bytes(),
    );

    // Document
    push_part(
        &mut mhtml,
        "text/html",
        document_charset,
        document_location,
        &html,
    );
    drop(html);
    out.write_all(&mhtml)?;
    drop(mhtml);

    // Assets
    let document_key: String = clean_url(document_url.clone()).to_string();
    let location_key: Option<String> =
        document_location.map(|url| clean_url(url.clone()).to_string());
    let mut packed: Vec<String> = Vec::new();

    for asset_url in session.asset_urls.clone().iter() {
        let mode = session.asset_mode(asset_url);
        let asset_url: Url = match Url::parse(asset_url) {
            Ok(asset_url) => clean_url(asset_url),
            Err(_) => continue,
        };

        // Data URLs carry their own contents; the document isn't an asset of itself
        if asset_url.scheme() == "data"
            || asset_url.as_str() == document_key
            || Some(asset_url.as_str()) == location_key.as_deref()
            || packed.iter().any(|url| url == asset_url.as_str())
        {
            continue;
        }

        // Errors are already reported by retrieve_asset(); assets that couldn't be retrieved
        // remain referenced by their full URLs in the document, the way they do in HTML output
        // What the page isn't allowed to use doesn't get packed (see cors.rs)
        if let Ok(Some((data, _final_url, media_type, charset))) =
            session.retrieve(document_url, &asset_url, mode)
        {
            // Source maps don't come along (see source_maps.rs)
            let essence = media_type.trim().to_ascii_lowercase();
            let data: &[u8] = match essence.ends_with("javascript")
                || essence.ends_with("ecmascript")
                || essence == "text/css"
            {
                true => remove_source_map_reference(&data),
                false => &data,
            };
            let mut part: Vec<u8> = vec![];
            push_part(&mut part, &media_type, &charset, Some(&asset_url), data);
            out.write_all(&part)?;
            packed.push(asset_url.to_string());
        }
    }

    // End of MIME message
    out.write_all(format!("--{}--\r\n", MHTML_BOUNDARY).as_bytes())
}

fn push_header(mhtml: &mut Vec<u8>, name: &str, value: &str) {
    mhtml.extend_from_slice(format!("{}: {}\r\n", name, value).as_bytes());
}

fn push_part(mhtml: &mut Vec<u8>, media_type: &str, charset: &str, url: Option<&Url>, data: &[u8]) {
    let is_text: bool = is_plaintext_media_type(media_type);

    mhtml.extend_from_slice(format!("--{}\r\n", MHTML_BOUNDARY).as_bytes());

    // Headers can't have line breaks in them
    let media_type: String = media_type.replace(['\r', '\n'], "");
    let charset: String = charset.replace(['\r', '\n', '"', '\\'], "");
    if is_text && !charset.is_empty() {
        push_header(
            mhtml,
            "Content-Type",
            &format!("{}; charset=\"{}\"", media_type, charset),
        );
    } else if !media_type.is_empty() {
        push_header(mhtml, "Content-Type", &media_type);
    }

    push_header(
        mhtml,
        "Content-Transfer-Encoding",
        if is_text {
            "quoted-printable"
        } else {
            "base64"
        },
    );

    if let Some(url) = url {
        if url.scheme().eq_ignore_ascii_case("cid") {
            push_header(
                mhtml,
                "Content-ID",
                &format!("<{}>", percent_decode(url.path())),
            );
        } else {
            push_header(mhtml, "Content-Location", url.as_str());
        }
    }

    mhtml.extend_from_slice(b"\r\n");
    if is_text {
        mhtml.extend_from_slice(encode_quoted_printable(data).as_bytes());
    } else {
        mhtml.extend_from_slice(encode_base64(data).as_bytes());
    }
    // The line break before the next boundary belongs to the boundary, not the part's contents
    mhtml.extend_from_slice(b"\r\n");
}

fn is_html_media_type(media_type: &str) -> bool {
    media_type.eq_ignore_ascii_case("text/html")
        || media_type.eq_ignore_ascii_case("application/xhtml+xml")
}

/// Returns the multipart boundary and the Content-ID of the root part ("start" parameter, if given).
/// Top-level MHTML archives are multipart/related or multipart/mixed; multipart/alternative only
/// appears as a node of a nested archive (see multipart_node_boundary).
fn multipart_boundary(headers: &Headers) -> Option<(String, Option<String>)> {
    let (media_type, params) = parse_content_type(get_header(headers, "content-type")?);

    if media_type != "multipart/related" && media_type != "multipart/mixed" {
        return None;
    }

    Some((boundary_of(&params)?, start_of(&params)))
}

/// Same as multipart_boundary(), but also accepting multipart/alternative: nodes nested inside
/// an MHTML archive can be of any of the three multipart types.
fn multipart_node_boundary(headers: &Headers) -> Option<(String, Option<String>)> {
    let (media_type, params) = parse_content_type(get_header(headers, "content-type")?);

    if media_type != "multipart/related"
        && media_type != "multipart/mixed"
        && media_type != "multipart/alternative"
    {
        return None;
    }

    Some((boundary_of(&params)?, start_of(&params)))
}

fn boundary_of(params: &HashMap<String, String>) -> Option<String> {
    params
        .get("boundary")
        .filter(|b| !b.is_empty())
        .cloned()
}

fn start_of(params: &HashMap<String, String>) -> Option<String> {
    params.get("start").map(|start| {
        start
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_string()
    })
}

fn get_header<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Splits a MIME entity into its headers and body.
/// When strict, returns None unless the data begins with a valid header (that's what tells MIME messages from HTML).
pub fn parse_headers(data: &[u8], strict: bool) -> Option<(Headers, &[u8])> {
    let mut headers: Headers = Vec::new();
    let mut pos: usize = 0;

    // Skip UTF-8 BOM
    if data.starts_with(b"\xEF\xBB\xBF") {
        pos = 3;
    }

    while pos < data.len() {
        let line_end: usize = data[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .unwrap_or(data.len());
        let next: usize = (line_end + 1).min(data.len());
        let line: &[u8] = data[pos..line_end]
            .strip_suffix(b"\r")
            .unwrap_or(&data[pos..line_end]);

        if line.is_empty() {
            // End of headers
            return Some((headers, &data[next..]));
        }

        if line[0] == b' ' || line[0] == b'\t' {
            // Folded header: continuation of the previous one
            match headers.last_mut() {
                Some((_, value)) => value.push_str(&String::from_utf8_lossy(line)),
                None if strict => return None,
                None => {}
            }
        } else {
            let colon: Option<usize> = line.iter().position(|&b| b == b':');
            let is_valid_name = |name: &[u8]| {
                !name.is_empty() && name.iter().all(|b| (33..=126).contains(b) && *b != b':')
            };

            match colon {
                Some(colon) if is_valid_name(&line[..colon]) => {
                    headers.push((
                        String::from_utf8_lossy(&line[..colon]).to_string(),
                        String::from_utf8_lossy(&line[colon + 1..])
                            .trim()
                            .to_string(),
                    ));
                }
                _ if strict && headers.is_empty() => return None,
                _ => {} // Skip malformed lines
            }
        }

        pos = next;
    }

    // Headers only, no body
    if strict && headers.is_empty() {
        None
    } else {
        Some((headers, &data[data.len()..]))
    }
}

/// Parses Content-Type header values: returns the media type (lowercase), and parameters (with lowercase names).
pub fn parse_content_type(value: &str) -> (String, HashMap<String, String>) {
    let mut params: HashMap<String, String> = HashMap::new();
    let chars: Vec<char> = value.chars().collect();
    let mut i: usize = 0;

    let mut media_type = String::new();
    while i < chars.len() && chars[i] != ';' {
        media_type.push(chars[i]);
        i += 1;
    }

    while i < chars.len() {
        i += 1; // Skip ";"

        let mut name = String::new();
        while i < chars.len() && chars[i] != '=' && chars[i] != ';' {
            name.push(chars[i]);
            i += 1;
        }
        if i >= chars.len() || chars[i] == ';' {
            continue; // Parameter without value
        }
        i += 1; // Skip "="
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }

        let mut value = String::new();
        if i < chars.len() && chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                value.push(chars[i]);
                i += 1;
            }
            // Skip whatever is between the closing quote and the next parameter
            while i < chars.len() && chars[i] != ';' {
                i += 1;
            }
        } else {
            while i < chars.len() && chars[i] != ';' {
                value.push(chars[i]);
                i += 1;
            }
            value = value.trim().to_string();
        }

        let name: String = name.trim().to_ascii_lowercase();
        if !name.is_empty() {
            params.entry(name).or_insert(value);
        }
    }

    (media_type.trim().to_ascii_lowercase(), params)
}

/// Splits the body of a multipart entity into parts (each one still has its headers).
pub fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let delimiter: Vec<u8> = format!("--{}", boundary).into_bytes();
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut part_start: Option<usize> = None;
    let mut pos: usize = 0;

    while pos < body.len() {
        let line_end: usize = body[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .unwrap_or(body.len());
        let line: &[u8] = &body[pos..line_end];

        if let Some(rest) = line.strip_prefix(delimiter.as_slice()) {
            // Delimiter lines may have whitespace ("transport padding") after them
            let rest: &[u8] = rest.trim_ascii();
            let is_last: bool = rest.starts_with(b"--");

            if rest.is_empty() || is_last {
                if let Some(start) = part_start {
                    // The line break before the delimiter is part of the delimiter
                    let mut end: usize = pos;
                    if end > start && body[end - 1] == b'\n' {
                        end -= 1;
                        if end > start && body[end - 1] == b'\r' {
                            end -= 1;
                        }
                    }
                    parts.push(&body[start..end]);
                }

                if is_last {
                    return parts;
                }

                part_start = Some((line_end + 1).min(body.len()));
            }
        }

        pos = line_end + 1;
    }

    // Truncated archive (no closing delimiter): keep what's there
    if let Some(start) = part_start {
        parts.push(&body[start..]);
    }

    parts
}

pub fn decode_base64(data: &[u8]) -> Vec<u8> {
    let data: Vec<u8> = data
        .iter()
        .filter(|b| !b.is_ascii_whitespace())
        .copied()
        .collect();

    BASE64_LENIENT.decode(&data).unwrap_or_default()
}

pub fn encode_base64(data: &[u8]) -> String {
    let encoded: String = BASE64_STANDARD.encode(data);
    let mut result: String =
        String::with_capacity(encoded.len() + encoded.len() / MAX_LINE_LENGTH * 2);

    // Base64 is pure ASCII, so it's safe to split it at any byte
    for (i, line) in encoded.as_bytes().chunks(MAX_LINE_LENGTH).enumerate() {
        if i > 0 {
            result.push_str("\r\n");
        }
        result.push_str(std::str::from_utf8(line).unwrap());
    }

    result
}

pub fn decode_quoted_printable(data: &[u8]) -> Vec<u8> {
    let mut result: Vec<u8> = Vec::with_capacity(data.len());
    let mut i: usize = 0;

    while i < data.len() {
        if data[i] != b'=' {
            result.push(data[i]);
            i += 1;
            continue;
        }

        // Soft line break: "=", optionally followed by whitespace, then end of line
        let mut j: usize = i + 1;
        while j < data.len() && (data[j] == b' ' || data[j] == b'\t') {
            j += 1;
        }
        if j >= data.len() {
            i = j;
            continue;
        }
        if data[j] == b'\n' {
            i = j + 1;
            continue;
        }
        if data[j] == b'\r' && data.get(j + 1) == Some(&b'\n') {
            i = j + 2;
            continue;
        }

        // Encoded byte
        if let Some(byte) = decode_hex_escape(data, i) {
            result.push(byte);
            i += 3;
            continue;
        }

        // Not valid quoted-printable, keep as is
        result.push(b'=');
        i += 1;
    }

    result
}

/// Encodes data as quoted-printable, preserving it byte for byte: CRLFs become hard line breaks,
/// lone LFs get encoded (followed by a soft line break, to keep the output readable).
/// Lines never begin with "-" or ".", so the output can't be mistaken for a MIME boundary (or SMTP terminator).
pub fn encode_quoted_printable(data: &[u8]) -> String {
    let mut result: String = String::with_capacity(data.len() + data.len() / 8);
    let mut line_length: usize = 0;
    let mut i: usize = 0;

    let is_literal = |b: u8, line_length: usize, is_before_line_end: bool| match b {
        b' ' | b'\t' => !is_before_line_end, // Trailing whitespace may get stripped in transit
        b'-' | b'.' => line_length > 0,
        b'=' => false,
        33..=126 => true,
        _ => false,
    };

    while i < data.len() {
        let b: u8 = data[i];

        if b == b'\r' && data.get(i + 1) == Some(&b'\n') {
            result.push_str("\r\n");
            line_length = 0;
            i += 2;
            continue;
        }

        let is_before_line_end: bool =
            i + 1 == data.len() || (data[i + 1] == b'\r' && data.get(i + 2) == Some(&b'\n'));
        let mut literal: bool = is_literal(b, line_length, is_before_line_end);

        // One spot is reserved for the soft line break's "="
        if line_length + if literal { 1 } else { 3 } > MAX_LINE_LENGTH - 1 {
            result.push_str("=\r\n");
            line_length = 0;
            literal = is_literal(b, line_length, is_before_line_end);
        }

        if literal {
            result.push(b as char);
            line_length += 1;
        } else {
            result.push_str(&format!("={:02X}", b));
            line_length += 3;
        }

        if b == b'\n' && i + 1 < data.len() {
            result.push_str("=\r\n");
            line_length = 0;
        }

        i += 1;
    }

    result
}

/// Makes text usable as a header value (RFC 2047 encoded-words if it's not plain ASCII).
fn encode_header_value(text: &str) -> String {
    // No line breaks or other control characters allowed
    let text: String = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<&str>>()
        .join(" ");

    if text.is_ascii() && !text.contains("=?") {
        return text;
    }

    // Encoded-words can't be longer than 75 characters: 45 bytes of text per word fit
    let mut words: Vec<String> = Vec::new();
    let mut chunk = String::new();
    for c in text.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(format!("=?utf-8?B?{}?=", BASE64_STANDARD.encode(&chunk)));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(format!("=?utf-8?B?{}?=", BASE64_STANDARD.encode(&chunk)));
    }

    words.join("\r\n ")
}

pub(crate) fn percent_decode(text: &str) -> String {
    let bytes: &[u8] = text.as_bytes();
    let mut result: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i: usize = 0;

    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = decode_hex_escape(bytes, i)
        {
            result.push(byte);
            i += 3;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }

    String::from_utf8_lossy(&result).to_string()
}

/// Decodes the two hex digits that follow position i ("=XX" in quoted-printable, "%XX" in URLs).
fn decode_hex_escape(data: &[u8], i: usize) -> Option<u8> {
    let hex_value = |b: &u8| (*b as char).to_digit(16).map(|d| d as u8);
    let high: u8 = hex_value(data.get(i + 1)?)?;
    let low: u8 = hex_value(data.get(i + 2)?)?;

    Some(high << 4 | low)
}

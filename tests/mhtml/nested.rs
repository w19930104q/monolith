//  ██████╗  █████╗ ███████╗███████╗██╗███╗   ██╗ ██████╗
//  ██╔══██╗██╔══██╗██╔════╝██╔════╝██║████╗  ██║██╔════╝
//  ██████╔╝███████║███████╗███████║██║██╔██╗ ██║██║  ███╗
//  ██╔═══╝ ██╔══██║╚════██║╚════██║██║╚██╗██║██║   ██║
//  ██║     ██║  ██║███████║███████║██║╚██████╔╝╚██████╔╝
//  ╚═╝     ╚═╝  ╚═╝╚══════╝╚══════╝╚═╝╚═╝  ╚═══╝ ╚═════╝

//! Nested MHTML unpacking (parse_nested_mhtml): recursive multipart trees, branch selection,
//! scoped Content-IDs, layered relative Content-Location resolution, and structural errors.

#[cfg(test)]
mod helpers {
    /// A part built out of its headers and body (CRLF line endings, as in real MIME messages).
    /// The blank line between headers and body is part of the part, the CRLF before the next
    /// delimiter belongs to the delimiter and gets added by message()/nested_part().
    pub fn part(headers: &str, body: &str) -> String {
        if headers.is_empty() {
            // A header-less part starts straight with its body
            body.to_string()
        } else {
            format!("{headers}\r\n\r\n{body}")
        }
    }

    /// Wraps parts into a multipart message with given Content-Type header value.
    /// The boundary name is "OUT": part delimiters are "--OUT", the closing one "--OUT--".
    pub fn message(content_type: &str, parts: &[String]) -> Vec<u8> {
        let mut out = format!(
            "MIME-Version: 1.0\r\nContent-Type: {content_type}\r\n\r\n"
        );
        for part in parts {
            out.push_str("--OUT\r\n");
            out.push_str(part);
            out.push_str("\r\n");
        }
        out.push_str("--OUT--\r\n");
        out.into_bytes()
    }

    pub fn html_part(location: Option<&str>, content_id: Option<&str>, body: &str) -> String {
        let mut headers = String::from("Content-Type: text/html; charset=utf-8");
        if let Some(location) = location {
            headers.push_str(&format!("\r\nContent-Location: {location}"));
        }
        if let Some(content_id) = content_id {
            headers.push_str(&format!("\r\nContent-ID: <{content_id}>"));
        }
        part(&headers, body)
    }

    pub fn leaf_part(
        media_type: &str,
        location: Option<&str>,
        content_id: Option<&str>,
        encoding: &str,
        body: &str,
    ) -> String {
        let mut headers = format!("Content-Type: {media_type}");
        if let Some(location) = location {
            headers.push_str(&format!("\r\nContent-Location: {location}"));
        }
        if let Some(content_id) = content_id {
            headers.push_str(&format!("\r\nContent-ID: <{content_id}>"));
        }
        headers.push_str(&format!("\r\nContent-Transfer-Encoding: {encoding}"));
        part(&headers, body)
    }

    /// An inner multipart message packed as a part (its own headers and delimited body).
    /// The boundary name is taken from the Content-Type value, so every nesting level can use its own.
    pub fn nested_part(content_type: &str, location: Option<&str>, parts: &[String]) -> String {
        let boundary: String = content_type
            .split(';')
            .find_map(|param| {
                let param = param.trim();
                param
                    .strip_prefix("boundary=")
                    .map(|value| value.trim_matches('"').to_string())
            })
            .unwrap_or_else(|| "IN".to_string());
        // The enclosing part only carries a Content-Location; its body is the whole nested message
        let mut headers = String::new();
        if let Some(location) = location {
            headers.push_str(&format!("Content-Location: {location}"));
        }
        // The nested message: its headers, the mandatory blank line, then its delimited body
        let mut body = format!("MIME-Version: 1.0\r\nContent-Type: {content_type}\r\n\r\n");
        for part in parts {
            body.push_str(&format!("--{boundary}\r\n"));
            body.push_str(part);
            body.push_str("\r\n");
        }
        body.push_str(&format!("--{boundary}--\r\n"));
        if headers.is_empty() {
            // A header-less enclosing part starts straight with the nested message
            body
        } else {
            let mut result = headers;
            // Blank line separating the enclosing part's headers from the nested message body
            result.push_str("\r\n");
            result.push_str(&body);
            result
        }
    }
}

#[cfg(test)]
mod passing {
    use url::Url;

    use monolith::mhtml::{is_mhtml, parse_mhtml, parse_nested_mhtml};

    use super::helpers::{html_part, leaf_part, message, nested_part};

    #[test]
    fn three_level_nesting_unpacks_every_leaf() {
        // Level 3: the deepest package, a related document referencing a CID image
        let level3 = nested_part(
            "multipart/related; boundary=\"L3\"; start=\"<doc3>\"",
            Some("https://a.test/deep/"),
            &[
                html_part(
                    Some("https://a.test/deep/page.html"),
                    Some("doc3"),
                    "<img src=\"cid:deep-img\">",
                ),
                leaf_part(
                    "image/png",
                    None,
                    Some("deep-img"),
                    "base64",
                    "REVFUFM=",
                ),
            ],
        );
        // Level 2: a mixed package holding the level-3 package and a sibling stylesheet
        let level2 = nested_part(
            "multipart/mixed; boundary=\"L2\"",
            Some("https://a.test/mid/"),
            &[
                leaf_part(
                    "text/css",
                    Some("https://a.test/mid/style.css"),
                    None,
                    "quoted-printable",
                    "p { color: red; }",
                ),
                level3,
            ],
        );
        // Level 1 (top level): a related package whose root is the level-2 package
        let data = message(
            "multipart/related; boundary=\"OUT\"; type=\"text/html\"",
            &[
                html_part(
                    Some("https://a.test/root.html"),
                    Some("top-doc"),
                    "<link rel=stylesheet href=\"mid/style.css\">\
                     <iframe src=\"deep/page.html\"></iframe>",
                ),
                level2,
            ],
        );

        assert!(is_mhtml(&data));
        let archive = parse_nested_mhtml(
            &data,
            Some(&Url::parse("file:///tmp/outer.mht").unwrap()),
        )
        .unwrap();
        assert!(archive.is_nested());

        // The root document comes from the deepest selected branch
        assert_eq!(
            archive.root().url.as_ref().unwrap().as_str(),
            "https://a.test/root.html"
        );

        // Every leaf across all three levels is retrievable
        let css = archive
            .get(&Url::parse("https://a.test/mid/style.css").unwrap())
            .unwrap();
        assert_eq!(css.media_type, "text/css");
        assert_eq!(css.data, b"p { color: red; }");

        let deep_doc = archive
            .get(&Url::parse("https://a.test/deep/page.html").unwrap())
            .unwrap();
        assert_eq!(deep_doc.media_type, "text/html");

        // The base64-encoded CID image of the deepest package got transfer-decoded
        let img = archive.get(&Url::parse("cid:deep-img").unwrap()).unwrap();
        assert_eq!(img.media_type, "image/png");
        assert_eq!(img.data, b"DEEPS");
    }

    #[test]
    fn mixed_picks_first_branch_leading_to_html() {
        // First branch has no document (just an image), second branch does
        let image_only = nested_part(
            "multipart/related; boundary=\"IN\"",
            None,
            &[leaf_part(
                "image/gif",
                Some("https://a.test/pic.gif"),
                None,
                "base64",
                "R0lG\r\n",
            )],
        );
        let with_doc = nested_part(
            "multipart/related; boundary=\"IN\"",
            None,
            &[html_part(
                Some("https://a.test/page.html"),
                Some("doc"),
                "<p>second branch</p>",
            )],
        );
        let data = message(
            "multipart/mixed; boundary=\"OUT\"",
            &[image_only, with_doc],
        );

        let archive = parse_nested_mhtml(&data, None).unwrap();
        assert_eq!(
            archive.root().url.as_ref().unwrap().as_str(),
            "https://a.test/page.html"
        );
        assert_eq!(archive.root().data, b"<p>second branch</p>");
        // The losing branch's asset is still available
        assert!(archive.contains(&Url::parse("https://a.test/pic.gif").unwrap()));
    }

    #[test]
    fn alternative_picks_the_last_part() {
        let data = message(
            "multipart/related; boundary=\"OUT\"",
            &[nested_part(
                "multipart/alternative; boundary=\"IN\"",
                Some("https://a.test/"),
                &[
                    html_part(Some("https://a.test/plain"), None, "<p>plain</p>"),
                    html_part(Some("https://a.test/rich"), None, "<p>rich</p>"),
                ],
            )],
        );

        let archive = parse_nested_mhtml(&data, None).unwrap();
        assert_eq!(
            archive.root().url.as_ref().unwrap().as_str(),
            "https://a.test/rich"
        );
    }

    #[test]
    fn related_start_names_direct_child() {
        let data = message(
            "multipart/related; boundary=\"OUT\"; start=\"<second>\"",
            &[
                html_part(Some("https://a.test/first"), Some("first"), "<p>1</p>"),
                html_part(Some("https://a.test/second"), Some("second"), "<p>2</p>"),
            ],
        );

        let archive = parse_nested_mhtml(&data, None).unwrap();
        assert_eq!(
            archive.root().url.as_ref().unwrap().as_str(),
            "https://a.test/second"
        );
    }

    #[test]
    fn related_without_start_takes_first_child() {
        let data = message(
            "multipart/related; boundary=\"OUT\"",
            &[
                html_part(Some("https://a.test/first"), Some("first"), "<p>1</p>"),
                html_part(Some("https://a.test/second"), Some("second"), "<p>2</p>"),
            ],
        );

        let archive = parse_nested_mhtml(&data, None).unwrap();
        assert_eq!(
            archive.root().url.as_ref().unwrap().as_str(),
            "https://a.test/first"
        );
    }

    #[test]
    fn same_cid_in_different_packages_resolves_per_scope() {
        // Two sibling related packages each define "logo", with different picture bytes
        let package_one = nested_part(
            "multipart/related; boundary=\"IN\"",
            Some("https://a.test/one/"),
            &[
                html_part(
                    Some("https://a.test/one/page.html"),
                    Some("doc1"),
                    "<img src=\"cid:logo\">",
                ),
                leaf_part("image/png", None, Some("logo"), "base64", "MQ==\r\n"),
            ],
        );
        let package_two = nested_part(
            "multipart/related; boundary=\"IN\"",
            Some("https://a.test/two/"),
            &[
                html_part(
                    Some("https://a.test/two/page.html"),
                    Some("doc2"),
                    "<img src=\"cid:logo\">",
                ),
                leaf_part("image/png", None, Some("logo"), "base64", "Mg==\r\n"),
            ],
        );
        let data = message(
            "multipart/mixed; boundary=\"OUT\"",
            &[package_one, package_two],
        );

        let archive = parse_nested_mhtml(&data, None).unwrap();

        // Each document resolves the shared CID to its own package's picture
        let one_doc = Url::parse("https://a.test/one/page.html").unwrap();
        let two_doc = Url::parse("https://a.test/two/page.html").unwrap();
        let cid = Url::parse("cid:logo").unwrap();

        assert_eq!(archive.get_for(&one_doc, &cid).unwrap().data, b"1");
        assert_eq!(archive.get_for(&two_doc, &cid).unwrap().data, b"2");
    }

    #[test]
    fn cid_falls_back_to_ancestor_package() {
        // The inner document asks for a CID only its grandparent package defines
        let inner = nested_part(
            "multipart/related; boundary=\"IN\"",
            Some("https://a.test/in/"),
            &[html_part(
                Some("https://a.test/in/page.html"),
                Some("inner-doc"),
                "<img src=\"cid:outer-logo\">",
            )],
        );
        let data = message(
            "multipart/related; boundary=\"OUT\"",
            &[
                inner,
                leaf_part(
                    "image/png",
                    Some("https://a.test/outer.png"),
                    Some("outer-logo"),
                    "base64",
                    "UVE=\r\n",
                ),
            ],
        );

        let archive = parse_nested_mhtml(&data, None).unwrap();
        let inner_doc = Url::parse("https://a.test/in/page.html").unwrap();
        let cid = Url::parse("cid:outer-logo").unwrap();
        assert_eq!(archive.get_for(&inner_doc, &cid).unwrap().data, b"QQ");
    }

    #[test]
    fn cid_never_resolves_from_sibling_package() {
        let defines_logo = nested_part(
            "multipart/related; boundary=\"IN\"",
            Some("https://a.test/has/"),
            &[
                html_part(
                    Some("https://a.test/has/page.html"),
                    Some("has-doc"),
                    "<img src=\"cid:logo\">",
                ),
                leaf_part("image/png", None, Some("logo"), "base64", "MQ==\r\n"),
            ],
        );
        let asks_for_logo = nested_part(
            "multipart/related; boundary=\"IN\"",
            Some("https://a.test/wants/"),
            &[html_part(
                Some("https://a.test/wants/page.html"),
                Some("wants-doc"),
                "<img src=\"cid:logo\">",
            )],
        );
        let data = message(
            "multipart/mixed; boundary=\"OUT\"",
            &[defines_logo, asks_for_logo],
        );

        let archive = parse_nested_mhtml(&data, None).unwrap();
        // The selected root is the first branch's document; the sibling's document must not
        // be able to see the first package's CID, since they aren't ancestors of one another
        let sibling_doc = Url::parse("https://a.test/wants/page.html").unwrap();
        let cid = Url::parse("cid:logo").unwrap();
        assert!(archive.get_for(&sibling_doc, &cid).is_none());
    }

    #[test]
    fn relative_content_locations_resolve_layer_by_layer() {
        // Inner parts use relative locations, resolved against the container part's location
        let inner = nested_part(
            "multipart/related; boundary=\"IN\"",
            Some("https://a.test/folder/inner.mht"),
            &[
                html_part(
                    Some("page.html"),
                    Some("inner-root"),
                    "<link rel=stylesheet href=\"style.css\">",
                ),
                leaf_part(
                    "text/css",
                    Some("style.css"),
                    None,
                    "7bit",
                    "body{}",
                ),
            ],
        );
        let data = message(
            "multipart/related; boundary=\"OUT\"",
            &[
                html_part(Some("https://a.test/folder/root.html"), None, "<p>x</p>"),
                inner,
            ],
        );

        let archive = parse_nested_mhtml(&data, None).unwrap();
        let css = archive
            .get(&Url::parse("https://a.test/folder/style.css").unwrap())
            .unwrap();
        assert_eq!(css.data, b"body{}");
        assert!(archive.contains(
            &Url::parse("https://a.test/folder/page.html").unwrap()
        ));
    }

    #[test]
    fn relative_locations_fall_back_to_archive_url() {
        // No part carries an absolute location: the fallback URL of the whole archive is the base
        let data = message(
            "multipart/related; boundary=\"OUT\"",
            &[
                html_part(Some("page.html"), Some("root"), "<img src=\"img.png\">"),
                leaf_part("image/png", Some("img.png"), None, "base64", "MQ==\r\n"),
            ],
        );
        let fallback = Url::parse("file:///tmp/site/archive.mht").unwrap();
        let archive = parse_nested_mhtml(&data, Some(&fallback)).unwrap();

        assert!(archive.contains(&Url::parse("file:///tmp/site/page.html").unwrap()));
        assert!(archive.contains(&Url::parse("file:///tmp/site/img.png").unwrap()));
    }

    #[test]
    fn quoted_printable_and_base64_parts_get_decoded() {
        let data = message(
            "multipart/related; boundary=\"OUT\"",
            &[
                html_part(
                    Some("https://a.test/"),
                    Some("root"),
                    "<p>ignored</p>",
                ),
                leaf_part(
                    "text/html",
                    Some("https://a.test/qp.html"),
                    None,
                    "quoted-printable",
                    "<p>caf=C3=A9 soft=\r\nbreak</p>",
                ),
                leaf_part(
                    "application/octet-stream",
                    Some("https://a.test/bin"),
                    None,
                    "base64",
                    "AAEC\r\nAw==\r\n",
                ),
            ],
        );

        let archive = parse_nested_mhtml(&data, None).unwrap();
        let qp = archive
            .get(&Url::parse("https://a.test/qp.html").unwrap())
            .unwrap();
        assert_eq!(qp.data, "<p>café softbreak</p>".as_bytes());
        let bin = archive
            .get(&Url::parse("https://a.test/bin").unwrap())
            .unwrap();
        assert_eq!(bin.data, &[0, 1, 2, 3]);
    }

    #[test]
    fn flat_mode_keeps_nested_message_packed() {
        // Without the nested flag, an embedded multipart part is treated as one opaque asset
        let inner = nested_part(
            "multipart/related; boundary=\"IN\"",
            Some("https://a.test/inner.mht"),
            &[html_part(Some("https://a.test/inner.html"), None, "<p>in</p>")],
        );
        let data = message(
            "multipart/related; boundary=\"OUT\"",
            &[
                html_part(Some("https://a.test/root.html"), None, "<p>root</p>"),
                inner,
            ],
        );

        let flat = parse_mhtml(&data, None).unwrap();
        assert!(!flat.is_nested());
        // Two top-level parts only: the root and the still-packed nested message
        assert_eq!(flat.len(), 2);
        assert!(flat
            .get(&Url::parse("https://a.test/inner.html").unwrap())
            .is_none());
    }
}

//  ███████╗ █████╗ ██╗██╗     ██╗███╗   ██╗ ██████╗
//  ██╔════╝██╔══██╗██║██║     ██║████╗  ██║██╔════╝
//  █████╗  ███████║██║     ██╔██╔██╗ ██║██║  ███╗
//  ██╔══╝  ██╔══██║██║     ██║╚██╗██║██║   ██║
//  ██║     ██║  ██║██║███████║██║ ╚██████╔╝╚██████╔╝
//  ╚═╝     ╚═╝  ╚═╝╚══════╝╚═╝╚═╝  ╚═══╝ ╚═════╝

#[cfg(test)]
mod failing {
    use monolith::mhtml::parse_nested_mhtml;

    use super::helpers::{html_part, leaf_part, message, nested_part};

    #[test]
    fn duplicate_cid_in_one_package_is_rejected() {
        let data = message(
            "multipart/related; boundary=\"OUT\"",
            &[
                html_part(Some("https://a.test/"), Some("root"), "<p>x</p>"),
                leaf_part("image/png", None, Some("dup"), "base64", "MQ==\r\n"),
                leaf_part("image/gif", None, Some("dup"), "base64", "Mg==\r\n"),
            ],
        );

        let error = parse_nested_mhtml(&data, None).unwrap_err();
        assert!(error.0.contains("duplicate Content-ID"), "{}", error.0);
        assert!(error.0.contains("dup"), "{}", error.0);
    }

    #[test]
    fn invalid_start_is_rejected() {
        // start names a Content-ID no direct child of the container has
        let data = message(
            "multipart/related; boundary=\"OUT\"; start=\"<missing>\"",
            &[html_part(Some("https://a.test/"), Some("root"), "<p>x</p>")],
        );

        let error = parse_nested_mhtml(&data, None).unwrap_err();
        assert!(error.0.contains("start"), "{}", error.0);
        assert!(error.0.contains("missing"), "{}", error.0);
    }

    #[test]
    fn mixed_without_any_html_is_rejected() {
        let data = message(
            "multipart/mixed; boundary=\"OUT\"",
            &[nested_part(
                "multipart/related; boundary=\"IN\"",
                None,
                &[leaf_part(
                    "image/png",
                    Some("https://a.test/a.png"),
                    None,
                    "base64",
                    "MQ==\r\n",
                )],
            )],
        );

        let error = parse_nested_mhtml(&data, None).unwrap_err();
        assert!(error.0.to_lowercase().contains("html"), "{}", error.0);
    }

    #[test]
    fn not_an_mhtml_message() {
        assert!(parse_nested_mhtml(b"<!DOCTYPE html><html></html>", None).is_err());
    }
}

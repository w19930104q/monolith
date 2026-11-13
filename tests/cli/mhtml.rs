//  ██████╗  █████╗ ███████╗███████╗██╗███╗   ██╗ ██████╗
//  ██╔══██╗██╔══██╗██╔════╝██╔════╝██║████╗  ██║██╔════╝
//  ██████╔╝███████║███████╗███████╗██║██╔██╗ ██║██║  ███╗
//  ██╔═══╝ ██╔══██║╚════██║╚════██║██║██║╚██╗██║██║   ██║
//  ██║     ██║  ██║███████║███████║██║██║ ╚████║╚██████╔╝
//  ╚═╝     ╚═╝  ╚═╝╚══════╝╚══════╝╚═╝╚═╝  ╚═══╝ ╚═════╝

#[cfg(test)]
mod passing {
    use assert_cmd::prelude::*;
    use std::path::MAIN_SEPARATOR;

    #[test]
    fn mhtml_file_target() {
        let mut cmd = crate::cli::monolith();
        let out = cmd
            .arg(format!(
                "tests{s}_data_{s}mhtml{s}chrome.mhtml",
                s = MAIN_SEPARATOR
            ))
            .output()
            .unwrap();

        let stdout = crate::cli::without_metadata(&out.stdout);
        assert!(stdout.starts_with("<!DOCTYPE html>"));
        assert!(stdout.contains(r#"<img src="data:image/png"#));
        assert!(stdout.contains(r#"<iframe src="data:text/html"#));
        assert!(!stdout.contains("cid:"));

        // Assets come from the archive, nothing gets requested over the network
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("https://example.com/page/style.css\n"));
        assert!(stderr.contains("https://example.com/missing.png (not found in archive)\n"));

        // Exit code should be 0
        out.assert().code(0);
    }
}

#[cfg(test)]
mod nested {
    use assert_cmd::prelude::*;

    /// Builds a MIME part (headers without trailing CRLF, then body).
    fn part(headers: &str, body: &str) -> String {
        if headers.is_empty() {
            body.to_string()
        } else {
            format!("{headers}\r\n\r\n{body}")
        }
    }

    /// A top-level multipart message (boundary name "OUT").
    fn message(content_type: &str, parts: &[String]) -> Vec<u8> {
        let mut out = format!("MIME-Version: 1.0\r\nContent-Type: {content_type}\r\n\r\n");
        for part in parts {
            out.push_str("--OUT\r\n");
            out.push_str(part);
            out.push_str("\r\n");
        }
        out.push_str("--OUT--\r\n");
        out.into_bytes()
    }

    fn html_part(location: &str, body: &str) -> String {
        part(
            &format!("Content-Type: text/html; charset=utf-8\r\nContent-Location: {location}"),
            body,
        )
    }

    fn leaf_part(
        media_type: &str,
        location: &str,
        encoding: &str,
        body: &str,
    ) -> String {
        part(
            &format!(
                "Content-Type: {media_type}\r\nContent-Location: {location}\r\nContent-Transfer-Encoding: {encoding}"
            ),
            body,
        )
    }

    /// A nested multipart message packed as a part (boundary name taken from its Content-Type).
    fn nested_part(content_type: &str, location: Option<&str>, parts: &[String]) -> String {
        let boundary: String = content_type
            .split(';')
            .find_map(|p| {
                p.trim()
                    .strip_prefix("boundary=")
                    .map(|v| v.trim_matches('"').to_string())
            })
            .unwrap();
        let headers = location
            .map(|l| format!("Content-Location: {l}"))
            .unwrap_or_default();
        let mut body = format!("MIME-Version: 1.0\r\nContent-Type: {content_type}\r\n\r\n");
        for part in parts {
            body.push_str(&format!("--{boundary}\r\n"));
            body.push_str(part);
            body.push_str("\r\n");
        }
        body.push_str(&format!("--{boundary}--\r\n"));
        part(&headers, &body)
    }

    fn run(archive: &[u8], extra_args: &[&str]) -> assert_cmd::assert::Assert {
        let mut command = crate::cli::monolith();
        command.args(extra_args).arg("-").write_stdin(archive.to_vec());
        command.assert()
    }

    #[test]
    fn extracts_nested_page_and_embeds_its_assets() {
        // An inner related package (the actual page) nested in the top-level related message
        let inner = nested_part(
            "multipart/related; boundary=\"IN\"",
            Some("https://n.test/folder/inner.mht"),
            &[
                html_part(
                    "https://n.test/folder/page.html",
                    "<title>Nested</title><link rel=stylesheet href=\"style.css\">\
                     <img src=\"img.png\">",
                ),
                leaf_part(
                    "text/css",
                    "https://n.test/folder/style.css",
                    "quoted-printable",
                    "body { background: url(\"bg.gif\"); }",
                ),
                leaf_part(
                    "image/png",
                    "https://n.test/folder/img.png",
                    "base64",
                    "REVFUFM=",
                ),
                leaf_part(
                    "image/gif",
                    "https://n.test/folder/bg.gif",
                    "base64",
                    "R0lG",
                ),
            ],
        );
        let archive = message(
            "multipart/related; boundary=\"OUT\"; type=\"text/html\"",
            &[inner],
        );

        let output = run(&archive, &["--nested"]);
        assert!(output.get_output().status.success());
        let stdout = crate::cli::without_metadata(&output.get_output().stdout);

        // The nested page is what got processed
        assert!(stdout.contains("<title>Nested</title>"));
        // HTML and CSS references both got embedded as data URLs, nothing stays remote or cid:
        assert!(stdout.contains(r#"<img src="data:image/png"#));
        assert!(stdout.contains(r#"url("data:image/gif"#) || stdout.contains("url(data:image/gif"));
        assert!(!stdout.contains("cid:"));
        assert!(!stdout.contains("https://n.test/folder/style.css"));
    }

    #[test]
    fn nested_mode_is_off_by_default() {
        let inner = nested_part(
            "multipart/related; boundary=\"IN\"",
            Some("https://n.test/inner.mht"),
            &[html_part("https://n.test/page.html", "<title>Inner</title>")],
        );
        let archive = message(
            "multipart/related; boundary=\"OUT\"",
            &[
                html_part("https://n.test/root.html", "<title>Outer</title>"),
                inner,
            ],
        );

        // Without --nested the nested message is an opaque asset, the outer root is the document
        let output = run(&archive, &[]);
        assert!(output.get_output().status.success());
        let stdout = crate::cli::without_metadata(&output.get_output().stdout);
        assert!(stdout.contains("<title>Outer</title>"));
        assert!(!stdout.contains("<title>Inner</title>"));
    }

    #[test]
    fn missing_referenced_asset_is_a_fatal_error() {
        // The nested page asks for an image none of the packages provide
        let inner = nested_part(
            "multipart/related; boundary=\"IN\"",
            Some("https://n.test/folder/"),
            &[html_part(
                "https://n.test/folder/page.html",
                "<title>Broken</title><img src=\"missing.png\">",
            )],
        );
        let archive = message(
            "multipart/related; boundary=\"OUT\"",
            &[inner],
        );

        // Nested mode never fetches the gap: it fails with a non-zero exit code and says so
        let output = run(&archive, &["--nested"]);
        assert!(!output.get_output().status.success());
        let stderr = String::from_utf8_lossy(&output.get_output().stderr);
        assert!(stderr.contains("missing from nested MHTML archive"), "{stderr}");
        assert!(stderr.contains("missing.png"), "{stderr}");
    }

    #[test]
    fn duplicate_content_id_is_rejected_with_nonzero_exit() {
        let archive = message(
            "multipart/related; boundary=\"OUT\"",
            &[
                part(
                    "Content-Type: text/html; charset=utf-8\r\nContent-Location: https://n.test/\r\nContent-ID: <root>",
                    "<img src=\"cid:logo\">",
                ),
                part(
                    "Content-Type: image/png\r\nContent-ID: <logo>\r\nContent-Transfer-Encoding: base64",
                    "MQ==",
                ),
                part(
                    "Content-Type: image/gif\r\nContent-ID: <logo>\r\nContent-Transfer-Encoding: base64",
                    "Mg==",
                ),
            ],
        );

        let output = run(&archive, &["--nested"]);
        assert!(!output.get_output().status.success());
        let stderr = String::from_utf8_lossy(&output.get_output().stderr);
        assert!(stderr.contains("duplicate Content-ID"), "{stderr}");
    }
}

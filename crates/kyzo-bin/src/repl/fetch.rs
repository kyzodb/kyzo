/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A minimal, hand-rolled HTTP(S) GET client for `%import <url>`
//! (`repl::commands::import`).
//!
//! The CozoDB original's `cozo-bin/src/client.rs` was an empty, licensed
//! placeholder — never referenced by anything, a stub for functionality
//! that never got written. This file is not a port of it; it is the client
//! `%import` actually needs, kept in the module its name always promised.
//!
//! This went through two designs before landing here, and both discarded
//! ones are worth recording because the reasons are load-bearing:
//!
//! 1. `minreq`'s `https-rustls` feature: rejected. It pulls `rustls` 0.21
//!    with its default crypto provider, `ring`, whose build script shells
//!    out to `cc` to compile C/assembly (verified: `cargo tree -e
//!    normal,build -i cc` showed `minreq -> rustls -> ring -> cc`).
//! 2. `ureq`'s `rustls-no-provider` feature plus `rustls-rustcrypto`
//!    installed as the process-default crypto provider: this worked and
//!    was pure Rust, but it depended on a *global* default
//!    (`rustls::crypto::CryptoProvider::install_default`, called once in
//!    `main.rs`) for `ureq`'s internals to find the provider — an
//!    invisible dependency between two unrelated modules for something
//!    this file can just own directly.
//!
//! What's here instead: `std::net::TcpStream` plus `rustls::ClientConnection`
//! doing a plain HTTP/1.1 GET by hand (~150 lines, most of it status-line/
//! header/chunked-body parsing that a client crate would otherwise hide),
//! with the crypto provider (`rustls-rustcrypto`, RustCrypto's pure-Rust
//! primitives — `aes-gcm`, `chacha20poly1305`, `p256`/`p384`, `rsa`, ...; no
//! `cc`/`cmake`/`bindgen`/`*-sys` crate anywhere in that tree, verified)
//! passed explicitly into this module's own `ClientConfig`, not installed
//! anywhere globally. `http://` and `https://` share every line of this
//! file except which `Transport` variant carries the bytes. Proven
//! end-to-end against a live server before being wired in here, not just
//! compiled.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use miette::{IntoDiagnostic, Result, bail, miette};

/// Redirects are real (an import URL is commonly a redirecting shortlink),
/// but must terminate: this is the same conservative cap most HTTP client
/// libraries default to.
const MAX_REDIRECTS: u32 = 5;

/// Fetch `url` (`http://` or `https://`) and return the response body as a
/// string, following up to [`MAX_REDIRECTS`] redirects.
pub fn get(url: &str) -> Result<String> {
    fetch(url, MAX_REDIRECTS)
}

fn fetch(url: &str, redirects_left: u32) -> Result<String> {
    let target = Target::parse(url)?;
    let transport = target.connect()?;
    let mut reader = BufReader::new(transport);
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: kyzo/{version}\r\n\
         Accept: */*\r\n\
         Connection: close\r\n\r\n",
        path = target.path_and_query,
        host = target.host,
        version = env!("CARGO_PKG_VERSION"),
    );
    reader
        .get_mut()
        .write_all(request.as_bytes())
        .into_diagnostic()?;

    let (status, headers) = read_status_and_headers(&mut reader)?;

    if (300..400).contains(&status) {
        let location = headers.get("location").ok_or_else(|| {
            miette!("redirect (HTTP {status}) from {url} carried no Location header")
        })?;
        if redirects_left == 0 {
            bail!("too many redirects fetching {url}");
        }
        return fetch(&target.resolve_redirect(location), redirects_left - 1);
    }
    if !(200..300).contains(&status) {
        bail!("HTTP {status} fetching {url}");
    }

    let body = read_body(&mut reader, &headers)?;
    String::from_utf8(body).into_diagnostic()
}

/// A parsed `http(s)://host[:port]/path?query` — just enough URL handling
/// for `%import`'s use case (no userinfo, no fragment, no IPv6 literal
/// brackets).
struct Target {
    https: bool,
    host: String,
    port: u16,
    path_and_query: String,
}

impl Target {
    fn parse(url: &str) -> Result<Self> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| miette!("URL has no scheme: {url}"))?;
        let https = match scheme {
            "http" => false,
            "https" => true,
            other => bail!("unsupported URL scheme '{other}' in {url}: only http/https"),
        };
        let (authority, path_and_query) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, "/".to_string()),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse()
                    .into_diagnostic()
                    .map_err(|e| miette!("bad port in {url}: {e}"))?,
            ),
            None => (authority.to_string(), if https { 443 } else { 80 }),
        };
        Ok(Target {
            https,
            host,
            port,
            path_and_query,
        })
    }

    /// `Location`'s value may be absolute or (commonly) a path relative to
    /// this same scheme/host/port.
    fn resolve_redirect(&self, location: &str) -> String {
        if location.starts_with("http://") || location.starts_with("https://") {
            location.to_string()
        } else {
            let scheme = if self.https { "https" } else { "http" };
            let path = if location.starts_with('/') {
                location.to_string()
            } else {
                format!("/{location}")
            };
            format!("{scheme}://{}:{}{}", self.host, self.port, path)
        }
    }

    fn connect(&self) -> Result<Transport> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port)).into_diagnostic()?;
        if !self.https {
            return Ok(Transport::Plain(tcp));
        }
        let root_store = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let config =
            rustls::ClientConfig::builder_with_provider(rustls_rustcrypto::provider().into())
                .with_safe_default_protocol_versions()
                .into_diagnostic()?
                .with_root_certificates(root_store)
                .with_no_client_auth();
        let server_name: rustls::pki_types::ServerName<'static> =
            self.host.clone().try_into().into_diagnostic()?;
        let conn =
            rustls::ClientConnection::new(Arc::new(config), server_name).into_diagnostic()?;
        Ok(Transport::Tls(Box::new(rustls::StreamOwned::new(
            conn, tcp,
        ))))
    }
}

/// The two byte-streams a `GET` can ride on. `http://` and `https://` share
/// every line of `fetch` except which variant this is.
enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.read(buf),
            Transport::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.write(buf),
            Transport::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(s) => s.flush(),
            Transport::Tls(s) => s.flush(),
        }
    }
}

/// Read the status line and headers of an HTTP/1.1 response. Header names
/// are lower-cased on the way in so lookups don't have to case-fold.
fn read_status_and_headers(reader: &mut impl BufRead) -> Result<(u16, HashMap<String, String>)> {
    let mut status_line = String::new();
    reader.read_line(&mut status_line).into_diagnostic()?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| miette!("malformed HTTP status line: {status_line:?}"))?
        .parse()
        .into_diagnostic()?;

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).into_diagnostic()?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok((status, headers))
}

/// Read the response body per `Content-Length`, per chunked
/// transfer-encoding, or (neither header present) to end-of-stream — which
/// this request's `Connection: close` guarantees terminates.
fn read_body(reader: &mut impl BufRead, headers: &HashMap<String, String>) -> Result<Vec<u8>> {
    if let Some(len) = headers.get("content-length") {
        let len: usize = len.parse().into_diagnostic()?;
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).into_diagnostic()?;
        Ok(buf)
    } else if headers
        .get("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        read_chunked_body(reader)
    } else {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).into_diagnostic()?;
        Ok(buf)
    }
}

fn read_chunked_body(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line).into_diagnostic()?;
        let Some(size_hex) = size_line.trim().split(';').next().map(str::trim) else {
            bail!("missing chunk size in chunked transfer encoding");
        };
        let size = usize::from_str_radix(size_hex, 16)
            .into_diagnostic()
            .map_err(|e| miette!("bad chunk size {size_hex:?}: {e}"))?;
        if size == 0 {
            // Trailing headers (rare) end at the next blank line.
            loop {
                let mut trailer = String::new();
                reader.read_line(&mut trailer).into_diagnostic()?;
                if trailer.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk).into_diagnostic()?;
        out.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).into_diagnostic()?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scheme_host_port_and_path() {
        let t = Target::parse("https://example.com:8443/a/b?c=d").unwrap();
        assert!(t.https);
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, 8443);
        assert_eq!(t.path_and_query, "/a/b?c=d");
    }

    #[test]
    fn defaults_port_by_scheme_and_path_to_root() {
        let http = Target::parse("http://example.com").unwrap();
        assert_eq!(http.port, 80);
        assert_eq!(http.path_and_query, "/");
        let https = Target::parse("https://example.com").unwrap();
        assert_eq!(https.port, 443);
    }

    #[test]
    fn rejects_unsupported_scheme() {
        assert!(Target::parse("ftp://example.com").is_err());
    }

    #[test]
    fn resolves_relative_and_absolute_redirects() {
        let t = Target::parse("https://example.com/old").unwrap();
        assert_eq!(t.resolve_redirect("/new"), "https://example.com:443/new");
        assert_eq!(
            t.resolve_redirect("http://other.example/x"),
            "http://other.example/x"
        );
    }

    #[test]
    fn reads_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let mut reader = BufReader::new(&raw[..]);
        let (status, headers) = read_status_and_headers(&mut reader).unwrap();
        assert_eq!(status, 200);
        let body = read_body(&mut reader, &headers).unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn reads_chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut reader = BufReader::new(&raw[..]);
        let (status, headers) = read_status_and_headers(&mut reader).unwrap();
        assert_eq!(status, 200);
        let body = read_body(&mut reader, &headers).unwrap();
        assert_eq!(body, b"hello world");
    }
}

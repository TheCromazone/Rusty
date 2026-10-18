//! Snapshot recipes and their build contexts.
//!
//! Daytona builds a snapshot from a Dockerfile plus context archives the
//! client puts in object storage first, exactly as SDK 0.220.0 does: each
//! `COPY` source is a tar at `<organization>/<md5>/context.tar` in the bucket
//! from `/object-storage/push-access`, and the snapshot request lists the MD5s.

use anyhow::{bail, Context as _, Result};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::api::{segment, PushAccess};
use super::digest::{hex, hmac_sha256, sha256_hex, Md5};

pub const BASE_IMAGE: &str =
    "debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251";
pub const BUILD_IMAGE: &str =
    "rust:1.90.0-alpine3.22@sha256:b4b54b176a74db7e5c68fdfe6029be39a02ccbcfe72b6e5a3e18e2c61b57ae26";
pub const PACKAGES: &[&str] = &["ca-certificates", "curl", "git", "make", "python3", "python3-venv", "ripgrep"];
/// Tracked inputs a source build needs; nothing else leaves the checkout.
pub const SOURCE_PATHS: &[&str] =
    &["Cargo.toml", "Cargo.lock", "src", "skills", "xtask", "LICENSE", "THIRD_PARTY_NOTICES.txt"];

fn apt() -> String {
    format!(
        "apt-get update -qq && apt-get install -y -qq --no-install-recommends {} && rm -rf /var/lib/apt/lists/*",
        PACKAGES.join(" ")
    )
}

/// One `COPY` source: a local file or directory and its name in the archive.
#[derive(Clone, Debug)]
pub struct Context {
    pub source: PathBuf,
    pub archive: String,
}

/// A Dockerfile and the local files it copies.
#[derive(Debug)]
pub struct Build {
    pub dockerfile: String,
    pub contexts: Vec<Context>,
}

/// Lexical `os.path.normpath` with the leading `/` removed, as the SDK names
/// a local file inside its context archive.
pub fn archive_path(p: &Path) -> String {
    let text = p.to_string_lossy();
    let mut parts: Vec<&str> = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|l| *l != "..") => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

fn local_file(path: &Path) -> Result<Context> {
    if !path.exists() {
        bail!("Local file {} does not exist", path.display());
    }
    if !path.is_file() {
        bail!("Local path {} exists but is not a file", path.display());
    }
    Ok(Context { source: path.into(), archive: archive_path(path) })
}

/// The snapshot for a prebuilt static binary and its memory companion.
/// LICENSE and THIRD_PARTY_NOTICES.txt are read from the current directory.
pub fn binary_build(binary: &Path, advisor: &Path) -> Result<Build> {
    let files = [
        (binary.to_path_buf(), "/usr/local/bin/rusty"),
        (advisor.to_path_buf(), "/usr/local/bin/rusty-memoryd"),
        (PathBuf::from("LICENSE"), "/usr/share/doc/rusty/LICENSE"),
        (PathBuf::from("THIRD_PARTY_NOTICES.txt"), "/usr/share/doc/rusty/THIRD_PARTY_NOTICES.txt"),
    ];
    let mut dockerfile = format!("FROM {BASE_IMAGE}\nRUN {}\n", apt());
    let mut contexts = Vec::new();
    for (local, remote) in files {
        let ctx = local_file(&local)?;
        dockerfile.push_str(&format!("COPY {} {remote}\n", ctx.archive));
        contexts.push(ctx);
    }
    dockerfile.push_str("RUN chmod 755 /usr/local/bin/rusty /usr/local/bin/rusty-memoryd && rusty --version\n");
    Ok(Build { dockerfile, contexts })
}

/// The multi-stage recipe that builds both binaries from tracked sources and
/// keeps Rust and build dependencies out of the runtime image.
pub fn source_recipe() -> String {
    [
        format!("FROM {BUILD_IMAGE} AS build"),
        "RUN apk add --no-cache gcc musl-dev".into(),
        "WORKDIR /src".into(),
        "COPY Cargo.toml Cargo.lock ./".into(),
        "COPY src src".into(),
        "COPY skills skills".into(),
        // The workspace manifest names xtask, so cargo needs its sources.
        "COPY xtask xtask".into(),
        "RUN cargo build --release --locked --bin rusty --bin rusty-memoryd && strip target/release/rusty target/release/rusty-memoryd".into(),
        format!("FROM {BASE_IMAGE}"),
        format!("RUN {}", apt()),
        "COPY --from=build /src/target/release/rusty /usr/local/bin/rusty".into(),
        "COPY --from=build /src/target/release/rusty-memoryd /usr/local/bin/rusty-memoryd".into(),
        "COPY LICENSE THIRD_PARTY_NOTICES.txt /usr/share/doc/rusty/".into(),
        "RUN rusty --version".into(),
        "CMD [\"sleep\", \"infinity\"]".into(),
    ]
    .join("\n")
}

/// The source build's contexts, from a directory holding the extracted
/// `git archive`. Every source must stay inside that directory.
pub fn source_build(dir: &Path) -> Result<Build> {
    let root = dir.canonicalize()?;
    let mut contexts = Vec::new();
    for name in SOURCE_PATHS {
        let source = dir.join(name);
        let real = source.canonicalize().with_context(|| format!("{name} is missing from the archive"))?;
        if !real.starts_with(&root) {
            bail!("forbidden path outside the build context: {}", real.display());
        }
        contexts.push(Context { source, archive: (*name).into() });
    }
    Ok(Build { dockerfile: source_recipe() + "\n", contexts })
}

/// Files under `dir` in a stable order: (path relative to `dir`, is_dir).
fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, bool)>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let kind = e.file_type()?;
        if kind.is_symlink() {
            bail!("refusing to archive a symlink: {}", e.path().display());
        }
        out.push((rel.clone(), kind.is_dir()));
        if kind.is_dir() {
            walk(&e.path(), &rel, out)?;
        }
    }
    Ok(())
}

/// The SDK's context id: MD5 of the archive name, then for a directory each
/// empty directory's relative path and each file's relative path and bytes.
pub fn context_hash(ctx: &Context) -> Result<String> {
    let mut h = Md5::default();
    h.update(ctx.archive.as_bytes());
    if ctx.source.is_file() {
        super::digest::hash_file(&ctx.source, |bytes| h.update(bytes))?;
        return Ok(h.hex());
    }
    let mut entries = Vec::new();
    walk(&ctx.source, "", &mut entries)?;
    if entries.is_empty() {
        h.update(b".");
    }
    for (rel, is_dir) in entries {
        let path = ctx.source.join(&rel);
        if is_dir {
            if std::fs::read_dir(&path)?.next().is_none() {
                h.update(rel.as_bytes());
            }
        } else {
            h.update(rel.as_bytes());
            super::digest::hash_file(&path, |bytes| h.update(bytes))?;
        }
    }
    Ok(h.hex())
}

// ------------------------------------------------------------------- tar

fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let s = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(&s.as_bytes()[s.len() - digits..]);
    field[digits] = 0;
}

fn tar_header(name: &str, size: u64, mode: u32, mtime: u64, dir: bool) -> Result<[u8; 512]> {
    let mut h = [0u8; 512];
    let bytes = name.as_bytes();
    let (prefix, base) = if bytes.len() <= 100 {
        (&b""[..], bytes)
    } else {
        // ustar: split at a '/' into a 155-byte prefix and a 100-byte name.
        let cut = (0..bytes.len())
            .rfind(|&i| bytes[i] == b'/' && i <= 155 && bytes.len() - i - 1 <= 100)
            .with_context(|| format!("path too long for a tar header: {name}"))?;
        (&bytes[..cut], &bytes[cut + 1..])
    };
    h[..base.len()].copy_from_slice(base);
    octal(&mut h[100..108], u64::from(mode & 0o7777));
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], mtime);
    h[148..156].fill(b' ');
    h[156] = if dir { b'5' } else { b'0' };
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[345..345 + prefix.len()].copy_from_slice(prefix);
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    Ok(h)
}

fn open_context_file(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NONBLOCK also prevents a concurrent replacement with a FIFO from
    // blocking open before the descriptor's type can be checked.
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    if !file.metadata()?.is_file() {
        bail!("refusing to archive {}: not a regular file", path.display());
    }
    Ok(file)
}

fn tar_entry(out: &mut impl Write, name: &str, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path)?;
    let mtime = meta.mtime().max(0) as u64;
    if meta.is_dir() {
        out.write_all(&tar_header(&format!("{name}/"), 0, meta.mode(), mtime, true)?)?;
    } else if meta.is_file() {
        let file = open_context_file(path)?;
        let meta = file.metadata()?;
        let size = meta.len();
        let mtime = meta.mtime().max(0) as u64;
        out.write_all(&tar_header(name, size, meta.mode(), mtime, false)?)?;
        if std::io::copy(&mut file.take(size), out)? != size {
            bail!("file changed while archiving: {}", path.display());
        }
        out.write_all(&[0u8; 512][..((512 - size % 512) % 512) as usize])?;
    } else {
        bail!("refusing to archive {}: not a regular file or directory", path.display());
    }
    Ok(())
}

/// A ustar archive of one context, named as the Dockerfile refers to it.
pub fn context_tar(ctx: &Context) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_context_tar(ctx, &mut out)?;
    Ok(out)
}

/// Write the identical archive to a stream, without retaining file contents.
pub fn write_context_tar(ctx: &Context, out: &mut impl Write) -> Result<()> {
    tar_entry(out, &ctx.archive, &ctx.source)?;
    if ctx.source.is_dir() {
        let mut entries = Vec::new();
        walk(&ctx.source, "", &mut entries)?;
        for (rel, _) in entries {
            tar_entry(out, &format!("{}/{rel}", ctx.archive), &ctx.source.join(&rel))?;
        }
    }
    out.write_all(&[0u8; 1024])?;
    Ok(())
}

// -------------------------------------------------------------- S3 upload

/// `YYYYMMDDTHHMMSSZ` for a Unix time.
pub fn amz_date(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// The AWS Signature Version 4 `Authorization` value. `headers` are
/// lower-case, sorted, and include every header that is signed.
#[allow(clippy::too_many_arguments)]
pub fn sigv4(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(&str, &str)],
    payload_sha256: &str,
    region: &str,
    access_key: &str,
    secret: &str,
    date: &str,
) -> String {
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
    let signed = signed.join(";");
    let canonical = format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload_sha256}");
    let day = &date[..8];
    let scope = format!("{day}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{date}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let mut key = hmac_sha256(format!("AWS4{secret}").as_bytes(), day.as_bytes());
    for part in [region, "s3", "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    let signature = hex(&hmac_sha256(&key, to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope},SignedHeaders={signed},Signature={signature}")
}

/// A path-style S3 request signed with the temporary push credentials.
fn s3(
    http: &reqwest::blocking::Client,
    access: &PushAccess,
    method: reqwest::Method,
    key: &str,
    body: Option<std::fs::File>,
    payload: &str,
) -> Result<reqwest::blocking::Response> {
    let base = reqwest::Url::parse(access.storage_url.trim_end_matches('/')).context("invalid storage URL")?;
    let prefix = base.path().trim_end_matches('/');
    let path =
        format!("{prefix}/{}/{}", segment(&access.bucket), key.split('/').map(segment).collect::<Vec<_>>().join("/"));
    let mut url = base.clone();
    url.set_path(&path);
    let host = match url.port() {
        Some(p) => format!("{}:{p}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    let date = amz_date(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs());
    let headers = [
        ("host", host.as_str()),
        ("x-amz-content-sha256", payload),
        ("x-amz-date", date.as_str()),
        ("x-amz-security-token", access.session_token.as_str()),
    ];
    let authorization =
        sigv4(method.as_str(), &path, "", &headers, payload, &access.region, &access.access_key, &access.secret, &date);
    let mut rb = http
        .request(method, url)
        .header("x-amz-content-sha256", payload)
        .header("x-amz-date", &date)
        .header("x-amz-security-token", &access.session_token)
        .header("Authorization", authorization)
        .timeout(Duration::from_secs(4 * 60));
    if let Some(body) = body {
        rb = rb.header("Content-Type", "application/x-tar").body(body);
    }
    rb.send().context("object storage request failed")
}

fn context_spool() -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = std::env::temp_dir().join(format!("rusty-context-{}", super::digest::random_hex(16)));
    let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
    std::fs::remove_file(path)?;
    Ok(file)
}

/// Hash exactly the bytes accepted by the spool, avoiding a second disk pass.
struct SignedSpool {
    file: std::fs::File,
    hash: super::digest::Sha256,
}

impl Write for SignedSpool {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn signed_context(ctx: &Context) -> Result<(std::fs::File, String)> {
    let spool = SignedSpool { file: context_spool()?, hash: super::digest::Sha256::default() };
    let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, spool);
    write_context_tar(ctx, &mut buffered)?;
    buffered.flush()?;
    let mut spool = buffered.into_inner().map_err(|e| e.into_error())?;
    spool.file.rewind()?;
    Ok((spool.file, spool.hash.hex()))
}

/// Uploads each context unless the bucket already has it; returns the MD5s
/// in Dockerfile order for the snapshot's `buildInfo.contextHashes`.
pub fn upload_contexts(access: &PushAccess, contexts: &[Context]) -> Result<Vec<String>> {
    let mut b = reqwest::blocking::Client::builder().connect_timeout(Duration::from_secs(30));
    if reqwest::Url::parse(&access.storage_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|h| h == "localhost" || h.parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback()))
    {
        b = b.no_proxy();
    }
    let http = b.build()?;
    let mut hashes = Vec::new();
    for ctx in contexts {
        let hash = context_hash(ctx)?;
        let key = format!("{}/{hash}/context.tar", access.organization_id);
        let exists = s3(&http, access, reqwest::Method::HEAD, &key, None, &sha256_hex(b""))?.status().is_success();
        if !exists {
            // A private, immediately unlinked spool bounds RAM while retaining
            // the exact bytes signed for S3, even if source files change later.
            let (file, hash) = signed_context(ctx)?;
            let resp = s3(&http, access, reqwest::Method::PUT, &key, Some(file), &hash)?;
            if !resp.status().is_success() {
                bail!("uploading {} failed with HTTP {}", ctx.archive, resp.status());
            }
        }
        hashes.push(hash);
    }
    Ok(hashes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_archives_preserve_bytes_padding_and_hashes() {
        let dir = std::env::temp_dir().join(format!("rusty-stream-tar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("input");
        for size in [0, 1, 511, 512, 513, 65535, 65536, 65537, 1048577] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            std::fs::write(&path, &data).unwrap();
            let ctx = Context { source: path.clone(), archive: "input".into() };
            let mut h = Md5::default();
            h.update(b"input");
            h.update(&data);
            assert_eq!(context_hash(&ctx).unwrap(), h.hex());
            let tar = context_tar(&ctx).unwrap();
            assert_eq!(&tar[512..512 + size], &data);
            assert_eq!(tar.len(), 512 + size.div_ceil(512) * 512 + 1024);
            assert!(tar[512 + size..].iter().all(|b| *b == 0));
            let (mut spool, hash) = signed_context(&ctx).unwrap();
            assert_eq!(hash, sha256_hex(&tar));
            let mut got = Vec::new();
            spool.read_to_end(&mut got).unwrap();
            assert_eq!(got, tar);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opened_context_rejects_symlinks_and_nonregular_files() {
        let dir = std::env::temp_dir().join(format!("rusty-no-follow-{}", super::super::digest::random_hex(8)));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, b"content").unwrap();
        assert!(open_context_file(&file).is_ok());
        let link = dir.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(open_context_file(&link).is_err());
        assert!(open_context_file(&dir).is_err());
        let fifo = dir.join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(open_context_file(&fifo).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn archive_names_follow_normpath() {
        assert_eq!(archive_path(Path::new("out/rusty")), "out/rusty");
        assert_eq!(archive_path(Path::new("./out//rusty")), "out/rusty");
        assert_eq!(archive_path(Path::new("/home/me/out/../bin/rusty")), "home/me/bin/rusty");
        assert_eq!(archive_path(Path::new("LICENSE")), "LICENSE");
    }

    #[test]
    fn dates_are_utc_civil_time() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1369353600), "20130524T000000Z");
        assert_eq!(amz_date(1709251199), "20240229T235959Z");
    }

    /// AWS's published "GET Object" example for Signature Version 4.
    #[test]
    fn sigv4_matches_the_aws_example() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let auth = sigv4(
            "GET",
            "/test.txt",
            "",
            &[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", empty),
                ("x-amz-date", "20130524T000000Z"),
            ],
            empty,
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "20130524T000000Z",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn tars_are_readable_by_tar_and_hashes_follow_contents() {
        let dir = std::env::temp_dir().join(format!("rusty-image-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ctx/src/nested")).unwrap();
        std::fs::create_dir_all(dir.join("ctx/src/empty")).unwrap();
        std::fs::write(dir.join("ctx/src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("ctx/src/nested/deep.rs"), "// deep\n").unwrap();
        let ctx = Context { source: dir.join("ctx/src"), archive: "src".into() };
        let tar = context_tar(&ctx).unwrap();
        assert_eq!(tar.len() % 512, 0);
        std::fs::write(dir.join("ctx.tar"), &tar).unwrap();
        let out = std::process::Command::new("tar").arg("tf").arg(dir.join("ctx.tar")).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let names = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            names.lines().collect::<Vec<_>>(),
            ["src/", "src/empty/", "src/main.rs", "src/nested/", "src/nested/deep.rs"]
        );
        let before = context_hash(&ctx).unwrap();
        assert_eq!(before.len(), 32);
        std::fs::write(dir.join("ctx/src/main.rs"), "fn main() { changed() }\n").unwrap();
        assert_ne!(before, context_hash(&ctx).unwrap());
        // A file context hashes its archive name and bytes.
        let file = Context { source: dir.join("ctx/src/nested/deep.rs"), archive: "deep.rs".into() };
        let mut h = Md5::default();
        h.update(b"deep.rs// deep\n");
        assert_eq!(context_hash(&file).unwrap(), h.hex());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_source_recipe_builds_the_workspace_it_copies() {
        let recipe = source_recipe();
        assert!(recipe.contains("COPY xtask xtask"), "the workspace manifest names xtask");
        assert!(recipe.contains(BUILD_IMAGE) && recipe.contains(BASE_IMAGE));
        assert!(BASE_IMAGE.contains("@sha256:") && BUILD_IMAGE.contains("@sha256:"));
        for path in SOURCE_PATHS {
            assert!(recipe.contains(path), "{path} is never copied");
        }
    }
}

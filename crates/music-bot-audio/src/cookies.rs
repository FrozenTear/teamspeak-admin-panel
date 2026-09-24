//! Private snapshots of the operator YouTube cookie jar.
//!
//! yt-dlp's `--cookies FILE` reads the Netscape jar and writes it back on
//! exit. Concurrent processes that share the uploaded file truncate and
//! interleave that write, which corrupts the jar the panel stored. Each
//! invocation gets its own temp copy. [`CookieJarCopy`]'s drop deletes the
//! copy. The upload is only read.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COOKIE_COPY_SEQ: AtomicU64 = AtomicU64::new(1);

/// A temp Netscape cookie file that yt-dlp may rewrite.
///
/// The operator upload is never passed as `--cookies`.
#[derive(Debug)]
pub struct CookieJarCopy {
    path: PathBuf,
}

impl CookieJarCopy {
    /// Snapshot `src` into a mode-0600 file under the temp dir.
    pub fn from_source(src: &Path) -> io::Result<Self> {
        let mut input = File::open(src)?;
        let path = unique_cookie_path();
        let mut output = create_private(&path)?;
        if let Err(err) = io::copy(&mut input, &mut output).and_then(|_| output.flush()) {
            let _ = fs::remove_file(&path);
            return Err(err);
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CookieJarCopy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn unique_cookie_path() -> PathBuf {
    let seq = COOKIE_COPY_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "yt-cookies-{}-{nanos}-{seq}.txt",
        std::process::id(),
    ))
}

fn create_private(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::CookieJarCopy;

    fn sample_jar() -> Vec<u8> {
        b"# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tabc\n".to_vec()
    }

    #[test]
    fn private_copies_do_not_rewrite_the_upload() {
        let dir = std::env::temp_dir().join(format!(
            "yt-cookies-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("yt-cookies.txt");
        let original = sample_jar();
        fs::write(&src, &original).unwrap();

        let first = CookieJarCopy::from_source(&src).unwrap();
        let second = CookieJarCopy::from_source(&src).unwrap();
        assert_ne!(first.path(), second.path());
        assert_ne!(first.path(), src.as_path());
        assert_ne!(second.path(), src.as_path());

        #[cfg(unix)]
        {
            let first_mode = fs::metadata(first.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                first_mode, 0o600,
                "cookie copy must not be group/world readable"
            );
        }

        // Simulate yt-dlp's exit write-back on one private jar.
        fs::write(first.path(), b"CORRUPT").unwrap();
        assert_eq!(fs::read(&src).unwrap(), original);
        assert_eq!(fs::read(second.path()).unwrap(), original);

        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();
        drop(first);
        drop(second);
        assert!(!first_path.exists());
        assert!(!second_path.exists());
        assert_eq!(fs::read(&src).unwrap(), original);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_upload_does_not_invent_a_jar() {
        let missing = std::env::temp_dir().join(format!(
            "yt-cookies-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_file(&missing);
        let err = CookieJarCopy::from_source(&missing).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn warm_resolver_cookie_self_check() {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/yt_resolver.py");
        let output = std::process::Command::new("python3")
            .arg(&script)
            .arg("--self-check-cookies")
            .output()
            .expect("python3");
        assert!(
            output.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("OK: private cookie copies"),
            "stderr:\n{stderr}"
        );
    }
}

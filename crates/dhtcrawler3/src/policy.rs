//! Loading the blocked-term list (design §9, R18): the built-in seed plus
//! `policy.terms_file`. Loading fails closed: any problem stops startup,
//! because an empty list would silently stop filtering.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dc3_policy::{PolicyError, SEED_TERMS, TermMatcher};

/// Largest `policy.terms_file` accepted.
pub const MAX_TERMS_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Why the blocked-term list could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum PolicyLoadError {
    #[error("the built-in blocked-term list is invalid: {0}")]
    Seed(PolicyError),
    #[error("cannot read policy.terms_file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("policy.terms_file {path} is larger than {max} bytes")]
    TooLarge { path: PathBuf, max: u64 },
    #[error("policy.terms_file {path} is not valid UTF-8")]
    NotUtf8 { path: PathBuf },
    #[error("policy.terms_file {path}: {source}")]
    Terms {
        path: PathBuf,
        #[source]
        source: PolicyError,
    },
    #[error("the blocked-term list is empty")]
    Empty,
    #[error("loading the blocked-term list failed: {0}")]
    Task(String),
}

fn read_terms_file(path: &Path) -> Result<String, PolicyLoadError> {
    let read_err = |source: std::io::Error| PolicyLoadError::Read {
        path: path.to_owned(),
        source,
    };
    let file = File::open(path).map_err(read_err)?;
    let mut bytes = Vec::new();
    file.take(MAX_TERMS_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(read_err)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_TERMS_FILE_BYTES {
        return Err(PolicyLoadError::TooLarge {
            path: path.to_owned(),
            max: MAX_TERMS_FILE_BYTES,
        });
    }
    String::from_utf8(bytes).map_err(|_| PolicyLoadError::NotUtf8 {
        path: path.to_owned(),
    })
}

/// The seed list plus `terms_file` (none when the path is empty).
pub fn load(terms_file: &Path) -> Result<TermMatcher, PolicyLoadError> {
    let seed = dc3_policy::try_seed().map_err(PolicyLoadError::Seed)?;
    let matcher = if terms_file.as_os_str().is_empty() {
        seed
    } else {
        let extra = read_terms_file(terms_file)?;
        let terms_err = |source| PolicyLoadError::Terms {
            path: terms_file.to_owned(),
            source,
        };
        // Checked alone first, so error line numbers match the file.
        TermMatcher::load(&extra).map_err(terms_err)?;
        TermMatcher::load(&format!("{SEED_TERMS}\n{extra}")).map_err(terms_err)?
    };
    if matcher.is_empty() {
        return Err(PolicyLoadError::Empty);
    }
    tracing::info!(terms = matcher.len(), "blocked-term list loaded");
    Ok(matcher)
}

/// [`load`] off the async threads, shared.
pub async fn load_shared(terms_file: PathBuf) -> Result<Arc<TermMatcher>, PolicyLoadError> {
    tokio::task::spawn_blocking(move || load(&terms_file))
        .await
        .map_err(|e| PolicyLoadError::Task(e.to_string()))?
        .map(Arc::new)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn seed_only_and_with_extra_terms() {
        let seed = load(Path::new("")).unwrap();
        assert!(seed.len() >= 10);
        assert!(seed.matches("some pthc thing"));
        assert!(!seed.matches("ubuntu"));

        let dir = tempfile::tempdir().unwrap();
        let extra = dir.path().join("terms.txt");
        std::fs::write(&extra, "# local additions\nforbiddenword\ntwo words\n").unwrap();
        let both = load(&extra).unwrap();
        assert_eq!(both.len(), seed.len() + 2);
        assert!(both.matches("a FORBIDDENWORD b"));
        assert!(both.matches("x two words y"));
        assert!(both.matches("pthc"), "the seed is still there");
    }

    #[test]
    fn problems_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.txt");
        assert!(matches!(load(&missing), Err(PolicyLoadError::Read { .. })));

        let bad = dir.path().join("bad.txt");
        std::fs::write(&bad, "fine\n!!!\n").unwrap();
        match load(&bad) {
            Err(PolicyLoadError::Terms { source, .. }) => {
                assert_eq!(source, PolicyError::EmptyTerm { line: 2 });
            }
            other => panic!("{other:?}"),
        }

        let binary = dir.path().join("binary.txt");
        std::fs::write(&binary, [0xff, 0xfe, b'\n']).unwrap();
        assert!(matches!(
            load(&binary),
            Err(PolicyLoadError::NotUtf8 { .. })
        ));

        // A directory cannot be read as a file.
        assert!(load(dir.path()).is_err());
    }
}

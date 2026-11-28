use crate::ReportError;
use std::{
    borrow::Cow,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub fn strip_verbatim_prefix(text: &str) -> Cow<'_, str> {
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        Cow::Borrowed(rest)
    } else {
        Cow::Borrowed(text)
    }
}

pub fn display_path(path: &Path) -> String {
    strip_verbatim_prefix(&path.to_string_lossy()).into_owned()
}

pub fn normalized_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(display_path(path))
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

pub fn is_redirect(info: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        info.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        info.file_type().is_symlink()
    }
}

pub fn reject_redirects(path: &Path, code: &'static str) -> Result<(), ReportError> {
    for part in path.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(info) => {
                if is_redirect(&info) {
                    return Err(ReportError::new(code, "路径包含链接或重定向"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn write_synced(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

pub fn read_bounded_bytes(
    path: &Path,
    limit: u64,
    redirect_code: &'static str,
    limit_code: &'static str,
) -> Result<Vec<u8>, ReportError> {
    reject_redirects(path, redirect_code)?;
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(ReportError::new(limit_code, "维护材料超过读取上限"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(ReportError::new(limit_code, "维护材料超过读取上限"));
    }
    Ok(bytes)
}

pub fn write_atomic(
    path: &Path,
    bytes: &[u8],
    redirect_code: &'static str,
    invalid_code: &'static str,
    missing_message: &str,
) -> Result<(), ReportError> {
    if path.file_name().is_none() {
        return Err(ReportError::new(invalid_code, missing_message));
    }
    let parent = path
        .parent()
        .ok_or_else(|| ReportError::new(invalid_code, missing_message))?;
    reject_redirects(path, redirect_code)?;
    fs::create_dir_all(parent)?;
    reject_redirects(path, redirect_code)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    write_synced(file.as_file_mut(), bytes)?;
    file.persist(path)
        .map_err(|error| ReportError::from(error.error))?;
    Ok(())
}

pub fn file_sha256(path: &Path, redirect_code: &'static str) -> Result<String, ReportError> {
    reject_redirects(path, redirect_code)?;
    Ok(daily_parsers::reader_sha256(File::open(path)?)?)
}

pub fn optional_file_sha256(
    path: &Path,
    redirect_code: &'static str,
) -> Result<Option<String>, ReportError> {
    reject_redirects(path, redirect_code)?;
    match File::open(path) {
        Ok(file) => Ok(Some(daily_parsers::reader_sha256(file)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::strip_verbatim_prefix;

    #[test]
    fn strip_verbatim_prefix_handles_local_unc_and_plain_paths() {
        assert_eq!(
            strip_verbatim_prefix(r"\\?\C:\work\a.txt"),
            r"C:\work\a.txt"
        );
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\server\share\a.txt"),
            r"\\server\share\a.txt"
        );
        assert_eq!(strip_verbatim_prefix(r"C:\work\a.txt"), r"C:\work\a.txt");
    }
}

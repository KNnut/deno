// Copyright 2018-2020 the Deno authors. All rights reserved. MIT license.

use crate::colors;
use crate::http_cache::HttpCache;
use crate::http_util::create_http_client;
use crate::http_util::fetch_once;
use crate::http_util::FetchOnceResult;
use crate::media_type::MediaType;
use crate::permissions::Permissions;
use crate::text_encoding;

use deno_core::error::custom_error;
use deno_core::error::generic_error;
use deno_core::error::uri_error;
use deno_core::error::AnyError;
use deno_core::futures;
use deno_core::futures::future::FutureExt;
use deno_core::ModuleSpecifier;
use deno_fetch::reqwest;
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::io::Read;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

const SUPPORTED_SCHEMES: [&str; 3] = ["http", "https", "file"];

/// A structure representing a source file.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct File {
  /// The path to the local version of the source file.  For local files this
  /// will be the direct path to that file.  For remote files, it will be the
  /// path to the file in the HTTP cache.
  pub local: PathBuf,
  /// For remote files, if there was an `X-TypeScript-Type` header, the parsed
  /// out value of that header.
  pub maybe_types: Option<String>,
  /// The resolved media type for the file.
  pub media_type: MediaType,
  /// The source of the file as a string.
  pub source: String,
  /// The _final_ specifier for the file.  The requested specifier and the final
  /// specifier maybe different for remote files that have been redirected.
  pub specifier: ModuleSpecifier,
}

/// Simple struct implementing in-process caching to prevent multiple
/// fs reads/net fetches for same file.
#[derive(Clone, Default)]
struct FileCache(Arc<Mutex<HashMap<ModuleSpecifier, File>>>);

impl FileCache {
  pub fn get(&self, specifier: &ModuleSpecifier) -> Option<File> {
    let cache = self.0.lock().unwrap();
    cache.get(specifier)
  }

  pub fn insert(&self, specifier: ModuleSpecifier, file: File) -> Option<File> {
    let mut cache = self.0.lock().unwrap();
    cache.insert(specifier, file)
  }
}

/// Indicates how cached source files should be handled.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum CacheSetting {
  /// Only the cached files should be used.  Any files not in the cache will
  /// error.  This is the equivalent of `--cached-only` in the CLI.
  Only,
  /// No cached source files should be used, and all files should be reloaded.
  /// This is the equivalent of `--reload` in the CLI.
  ReloadAll,
  /// Only some cached resources should be used.  This is the equivalent of
  /// `--reload=https://deno.land/std` or
  /// `--reload=https://deno.land/std,https://deno.land/x/example`.
  ReloadSome(Vec<String>),
  /// The cached source files should be used for local modules.  This is the
  /// default behavior of the CLI.
  Use,
}

impl CacheSetting {
  /// Returns if the cache should be used for a given specifier.
  pub fn should_use(&self, specifier: &ModuleSpecifier) -> bool {
    match self {
      CacheSetting::ReloadAll => false,
      CacheSetting::Use | CacheSetting::Only => true,
      CacheSetting::ReloadSome(list) => {
        let mut url = specifier.as_url().clone();
        url.set_fragment(None);
        if list.contains(&url.as_str().to_string()) {
          return false;
        }
        url.set_query(None);
        let mut path = PathBuf::from(url.as_str());
        loop {
          if list.contains(&path.to_str().unwrap().to_string()) {
            return false;
          }
          if !path.pop() {
            break;
          }
        }
        true
      }
    }
  }
}

/// Fetch a source file from the local file system.
fn fetch_local(specifier: &ModuleSpecifier) -> Result<File, AnyError> {
  let local = specifier.as_url().to_file_path().map_err(|_| {
    uri_error(format!("Invalid file path.\n  Specifier: {}", specifier))
  })?;
  let bytes = fs::read(local.clone())?;
  let charset = text_encoding::detect_charset(&bytes).to_string();
  let source = strip_shebang(get_source_from_bytes(bytes, Some(charset))?);
  let media_type = MediaType::from(specifier);

  Ok(File {
    local,
    maybe_types: None,
    media_type,
    source,
    specifier: specifier.clone(),
  })
}

/// Given a vector of bytes and optionally a charset, decode the bytes to a
/// string.
fn get_source_from_bytes(
  bytes: Vec<u8>,
  maybe_charset: Option<String>,
) -> Result<String, AnyError> {
  let source = if let Some(charset) = maybe_charset {
    text_encoding::convert_to_utf8(&bytes, &charset)?.to_string()
  } else {
    String::from_utf8(bytes)?
  };

  Ok(source)
}

/// Return a validated scheme for a given module specifier.
fn get_validated_scheme(
  specifier: &ModuleSpecifier,
) -> Result<String, AnyError> {
  let scheme = specifier.as_url().scheme();
  if !SUPPORTED_SCHEMES.contains(&scheme) {
    Err(generic_error(format!(
      "Unsupported scheme \"{}\" for module \"{}\". Supported schemes: {:#?}",
      scheme, specifier, SUPPORTED_SCHEMES
    )))
  } else {
    Ok(scheme.to_string())
  }
}

/// Resolve a media type and optionally the charset from a module specifier and
/// the value of a content type header.
fn map_content_type(
  specifier: &ModuleSpecifier,
  maybe_content_type: Option<String>,
) -> (MediaType, Option<String>) {
  if let Some(content_type) = maybe_content_type {
    let mut content_types = content_type.split(';');
    let content_type = content_types.next().unwrap();
    let media_type = match content_type.trim().to_lowercase().as_ref() {
      "application/typescript"
      | "text/typescript"
      | "video/vnd.dlna.mpeg-tts"
      | "video/mp2t"
      | "application/x-typescript" => {
        map_js_like_extension(specifier, MediaType::TypeScript)
      }
      "application/javascript"
      | "text/javascript"
      | "application/ecmascript"
      | "text/ecmascript"
      | "application/x-javascript"
      | "application/node" => {
        map_js_like_extension(specifier, MediaType::JavaScript)
      }
      "application/json" | "text/json" => MediaType::Json,
      "application/wasm" => MediaType::Wasm,
      // Handle plain and possibly webassembly
      "text/plain" | "application/octet-stream" => MediaType::from(specifier),
      _ => {
        debug!("unknown content type: {}", content_type);
        MediaType::Unknown
      }
    };
    let charset = content_types
      .map(str::trim)
      .find_map(|s| s.strip_prefix("charset="))
      .map(String::from);

    (media_type, charset)
  } else {
    (MediaType::from(specifier), None)
  }
}

/// Used to augment media types by using the path part of a module specifier to
/// resolve to a more accurate media type.
fn map_js_like_extension(
  specifier: &ModuleSpecifier,
  default: MediaType,
) -> MediaType {
  let url = specifier.as_url();
  let path = if url.scheme() == "file" {
    if let Ok(path) = url.to_file_path() {
      path
    } else {
      PathBuf::from(url.path())
    }
  } else {
    PathBuf::from(url.path())
  };
  match path.extension() {
    None => default,
    Some(os_str) => match os_str.to_str() {
      None => default,
      Some("jsx") => MediaType::JSX,
      Some("tsx") => MediaType::TSX,
      // Because DTS files do not have a separate media type, or a unique
      // extension, we have to "guess" at those things that we consider that
      // look like TypeScript, and end with `.d.ts` are DTS files.
      Some("ts") => {
        if default == MediaType::TypeScript {
          match path.file_stem() {
            None => default,
            Some(os_str) => {
              if let Some(file_stem) = os_str.to_str() {
                if file_stem.ends_with(".d") {
                  MediaType::Dts
                } else {
                  default
                }
              } else {
                default
              }
            }
          }
        } else {
          default
        }
      }
      Some(_) => default,
    },
  }
}

/// Remove shebangs from the start of source code strings
fn strip_shebang(mut value: String) -> String {
  if value.starts_with("#!") {
    if let Some(mid) = value.find('\n') {
      let (_, rest) = value.split_at(mid);
      value = rest.to_string()
    } else {
      value.clear()
    }
  }
  value
}

/// A structure for resolving, fetching and caching source files.
#[derive(Clone)]
pub struct FileFetcher {
  allow_remote: bool,
  cache: FileCache,
  cache_setting: CacheSetting,
  http_cache: HttpCache,
  http_client: reqwest::Client,
}

impl FileFetcher {
  pub fn new(
    http_cache: HttpCache,
    cache_setting: CacheSetting,
    allow_remote: bool,
    maybe_ca_file: Option<&str>,
  ) -> Result<Self, AnyError> {
    Ok(Self {
      allow_remote,
      cache: FileCache::default(),
      cache_setting,
      http_cache,
      http_client: create_http_client(maybe_ca_file)?,
    })
  }

  /// Creates a `File` structure for a remote file.
  fn build_remote_file(
    &self,
    specifier: &ModuleSpecifier,
    bytes: Vec<u8>,
    headers: &HashMap<String, String>,
  ) -> Result<File, AnyError> {
    let local = self.http_cache.get_cache_filename(specifier.as_url());
    let maybe_content_type = headers.get("content-type").cloned();
    let (media_type, maybe_charset) =
      map_content_type(specifier, maybe_content_type);
    let source = strip_shebang(get_source_from_bytes(bytes, maybe_charset)?);
    let maybe_types = headers.get("x-typescript-types").cloned();

    Ok(File {
      local,
      maybe_types,
      media_type,
      source,
      specifier: specifier.clone(),
    })
  }

  /// Fetch cached remote file.
  ///
  /// This is a recursive operation if source file has redirections.
  fn fetch_cached(
    &self,
    specifier: &ModuleSpecifier,
    redirect_limit: i64,
  ) -> Result<Option<File>, AnyError> {
    debug!("FileFetcher::fetch_cached - specifier: {}", specifier);
    if redirect_limit < 0 {
      return Err(custom_error("Http", "Too many redirects."));
    }

    let (mut source_file, headers) =
      match self.http_cache.get(specifier.as_url()) {
        Err(err) => {
          if let Some(err) = err.downcast_ref::<std::io::Error>() {
            if err.kind() == std::io::ErrorKind::NotFound {
              return Ok(None);
            }
          }
          return Err(err);
        }
        Ok(cache) => cache,
      };
    if let Some(redirect_to) = headers.get("location") {
      let redirect =
        ModuleSpecifier::resolve_import(redirect_to, specifier.as_str())?;
      return self.fetch_cached(&redirect, redirect_limit - 1);
    }
    let mut bytes = Vec::new();
    source_file.read_to_end(&mut bytes)?;
    let file = self.build_remote_file(specifier, bytes, &headers)?;

    Ok(Some(file))
  }

  /// Asynchronously fetch remote source file specified by the URL following
  /// redirects.
  ///
  /// **Note** this is a recursive method so it can't be "async", but needs to
  /// return a `Pin<Box<..>>`.
  fn fetch_remote(
    &self,
    specifier: &ModuleSpecifier,
    permissions: &Permissions,
    redirect_limit: i64,
  ) -> Pin<Box<dyn Future<Output = Result<File, AnyError>>>> {
    debug!("FileFetcher::fetch_remote() - specifier: {}", specifier);
    if redirect_limit < 0 {
      return futures::future::err(custom_error("Http", "Too many redirects."))
        .boxed_local();
    }

    if let Err(err) = permissions.check_specifier(specifier) {
      return futures::future::err(err).boxed_local();
    }

    if self.cache_setting.should_use(specifier) {
      match self.fetch_cached(specifier, redirect_limit) {
        Ok(Some(file)) => {
          return futures::future::ok(file).boxed_local();
        }
        Ok(None) => {}
        Err(err) => {
          return futures::future::err(err).boxed_local();
        }
      }
    }

    if self.cache_setting == CacheSetting::Only {
      return futures::future::err(custom_error(
        "NotFound",
        format!(
          "Specifier not found in cache: \"{}\", --cached-only is specified.",
          specifier
        ),
      ))
      .boxed_local();
    }

    info!("{} {}", colors::green("Download"), specifier);

    let file_fetcher = self.clone();
    let cached_etag = match self.http_cache.get(specifier.as_url()) {
      Ok((_, headers)) => headers.get("etag").cloned(),
      _ => None,
    };
    let specifier = specifier.clone();
    let permissions = permissions.clone();
    let http_client = self.http_client.clone();
    // A single pass of fetch either yields code or yields a redirect.
    async move {
      match fetch_once(http_client, specifier.as_url(), cached_etag).await? {
        FetchOnceResult::NotModified => {
          let file = file_fetcher.fetch_cached(&specifier, 10)?.unwrap();
          Ok(file)
        }
        FetchOnceResult::Redirect(redirect_url, headers) => {
          file_fetcher
            .http_cache
            .set(specifier.as_url(), headers, &[])?;
          let redirect_specifier = ModuleSpecifier::from(redirect_url);
          file_fetcher
            .fetch_remote(&redirect_specifier, &permissions, redirect_limit - 1)
            .await
        }
        FetchOnceResult::Code(bytes, headers) => {
          file_fetcher.http_cache.set(
            specifier.as_url(),
            headers.clone(),
            &bytes,
          )?;
          let file =
            file_fetcher.build_remote_file(&specifier, bytes, &headers)?;
          Ok(file)
        }
      }
    }
    .boxed_local()
  }

  /// Fetch a source file and asynchronously return it.
  pub async fn fetch(
    &self,
    specifier: &ModuleSpecifier,
    permissions: &Permissions,
  ) -> Result<File, AnyError> {
    debug!("FileFetcher::fetch() - specifier: {}", specifier);
    let scheme = get_validated_scheme(specifier)?;
    permissions.check_specifier(specifier)?;
    if let Some(file) = self.cache.get(specifier) {
      Ok(file)
    } else {
      let is_local = scheme == "file";

      let result = if is_local {
        fetch_local(specifier)
      } else if !self.allow_remote {
        Err(custom_error(
          "NoRemote",
          format!("A remote specifier was requested: \"{}\", but --no-remote is specified.", specifier),
        ))
      } else {
        self.fetch_remote(specifier, permissions, 10).await
      };

      if let Ok(file) = &result {
        self.cache.insert(specifier.clone(), file.clone());
      }

      result
    }
  }

  /// Get a previously in memory cached file.
  pub fn get_cached(&self, specifier: &ModuleSpecifier) -> Option<File> {
    self.cache.get(specifier)
  }

  /// Get the location of the current HTTP cache associated with the fetcher.
  pub fn get_http_cache_location(&self) -> PathBuf {
    self.http_cache.location.clone()
  }

  /// Insert a temporary module into the in memory cache for the file fetcher.
  pub fn insert_cached(&self, file: File) -> Option<File> {
    self.cache.insert(file.specifier.clone(), file)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use deno_core::error::get_custom_error_class;
  use std::rc::Rc;
  use tempfile::TempDir;

  fn setup(
    cache_setting: CacheSetting,
    maybe_temp_dir: Option<Rc<TempDir>>,
  ) -> (FileFetcher, Rc<TempDir>) {
    let temp_dir = maybe_temp_dir.unwrap_or_else(|| {
      Rc::new(TempDir::new().expect("failed to create temp directory"))
    });
    let location = temp_dir.path().join("deps");
    let file_fetcher =
      FileFetcher::new(HttpCache::new(&location), cache_setting, true, None)
        .expect("setup failed");
    (file_fetcher, temp_dir)
  }

  macro_rules! file_url {
    ($path:expr) => {
      if cfg!(target_os = "windows") {
        concat!("file:///C:", $path)
      } else {
        concat!("file://", $path)
      }
    };
  }

  async fn test_fetch(specifier: &ModuleSpecifier) -> (File, FileFetcher) {
    let (file_fetcher, _) = setup(CacheSetting::ReloadAll, None);
    let result = file_fetcher
      .fetch(specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());
    (result.unwrap(), file_fetcher)
  }

  async fn test_fetch_remote(
    specifier: &ModuleSpecifier,
  ) -> (File, HashMap<String, String>) {
    let _http_server_guard = test_util::http_server();
    let (file_fetcher, _) = setup(CacheSetting::ReloadAll, None);
    let result: Result<File, AnyError> = file_fetcher
      .fetch_remote(specifier, &Permissions::allow_all(), 1)
      .await;
    assert!(result.is_ok());
    let (_, headers) = file_fetcher.http_cache.get(specifier.as_url()).unwrap();
    (result.unwrap(), headers)
  }

  async fn test_fetch_remote_encoded(
    fixture: &str,
    charset: &str,
    expected: &str,
  ) {
    let url_str =
      format!("http://127.0.0.1:4545/cli/tests/encoding/{}", fixture);
    let specifier = ModuleSpecifier::resolve_url(&url_str).unwrap();
    let (file, headers) = test_fetch_remote(&specifier).await;
    assert_eq!(file.source, expected);
    assert_eq!(file.media_type, MediaType::TypeScript);
    assert_eq!(
      headers.get("content-type").unwrap(),
      &format!("application/typescript;charset={}", charset)
    );
  }

  async fn test_fetch_local_encoded(charset: &str, expected: String) {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join(format!("tests/encoding/{}.ts", charset));
    let specifier =
      ModuleSpecifier::resolve_url_or_path(p.to_str().unwrap()).unwrap();
    let (file, _) = test_fetch(&specifier).await;
    assert_eq!(file.source, expected);
  }

  #[test]
  fn test_get_validated_scheme() {
    let fixtures = vec![
      ("https://deno.land/x/mod.ts", true, "https"),
      ("http://deno.land/x/mod.ts", true, "http"),
      ("file:///a/b/c.ts", true, "file"),
      ("file:///C:/a/b/c.ts", true, "file"),
      ("ftp://a/b/c.ts", false, ""),
      ("mailto:dino@deno.land", false, ""),
    ];

    for (specifier, is_ok, expected) in fixtures {
      let specifier = ModuleSpecifier::resolve_url_or_path(specifier).unwrap();
      let actual = get_validated_scheme(&specifier);
      assert_eq!(actual.is_ok(), is_ok);
      if is_ok {
        assert_eq!(actual.unwrap(), expected);
      }
    }
  }

  #[test]
  fn test_strip_shebang() {
    let value =
      "#!/usr/bin/env deno\n\nconsole.log(\"hello deno!\");\n".to_string();
    assert_eq!(strip_shebang(value), "\n\nconsole.log(\"hello deno!\");\n");
  }

  #[test]
  fn test_map_content_type() {
    let fixtures = vec![
      // Extension only
      (file_url!("/foo/bar.ts"), None, MediaType::TypeScript, None),
      (file_url!("/foo/bar.tsx"), None, MediaType::TSX, None),
      (file_url!("/foo/bar.d.ts"), None, MediaType::Dts, None),
      (file_url!("/foo/bar.js"), None, MediaType::JavaScript, None),
      (file_url!("/foo/bar.jsx"), None, MediaType::JSX, None),
      (file_url!("/foo/bar.json"), None, MediaType::Json, None),
      (file_url!("/foo/bar.wasm"), None, MediaType::Wasm, None),
      (file_url!("/foo/bar.cjs"), None, MediaType::JavaScript, None),
      (file_url!("/foo/bar.mjs"), None, MediaType::JavaScript, None),
      (file_url!("/foo/bar"), None, MediaType::Unknown, None),
      // Media type no extension
      (
        "https://deno.land/x/mod",
        Some("application/typescript".to_string()),
        MediaType::TypeScript,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("text/typescript".to_string()),
        MediaType::TypeScript,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("video/vnd.dlna.mpeg-tts".to_string()),
        MediaType::TypeScript,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("video/mp2t".to_string()),
        MediaType::TypeScript,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("application/x-typescript".to_string()),
        MediaType::TypeScript,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("application/javascript".to_string()),
        MediaType::JavaScript,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("text/javascript".to_string()),
        MediaType::JavaScript,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("application/ecmascript".to_string()),
        MediaType::JavaScript,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("text/ecmascript".to_string()),
        MediaType::JavaScript,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("application/x-javascript".to_string()),
        MediaType::JavaScript,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("application/node".to_string()),
        MediaType::JavaScript,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("text/json".to_string()),
        MediaType::Json,
        None,
      ),
      (
        "https://deno.land/x/mod",
        Some("text/json; charset=utf-8".to_string()),
        MediaType::Json,
        Some("utf-8".to_string()),
      ),
      // Extension with media type
      (
        "https://deno.land/x/mod.ts",
        Some("text/plain".to_string()),
        MediaType::TypeScript,
        None,
      ),
      (
        "https://deno.land/x/mod.ts",
        Some("foo/bar".to_string()),
        MediaType::Unknown,
        None,
      ),
      (
        "https://deno.land/x/mod.tsx",
        Some("application/typescript".to_string()),
        MediaType::TSX,
        None,
      ),
      (
        "https://deno.land/x/mod.tsx",
        Some("application/javascript".to_string()),
        MediaType::TSX,
        None,
      ),
      (
        "https://deno.land/x/mod.jsx",
        Some("application/javascript".to_string()),
        MediaType::JSX,
        None,
      ),
      (
        "https://deno.land/x/mod.jsx",
        Some("application/x-typescript".to_string()),
        MediaType::JSX,
        None,
      ),
      (
        "https://deno.land/x/mod.d.ts",
        Some("application/javascript".to_string()),
        MediaType::JavaScript,
        None,
      ),
      (
        "https://deno.land/x/mod.d.ts",
        Some("text/plain".to_string()),
        MediaType::Dts,
        None,
      ),
      (
        "https://deno.land/x/mod.d.ts",
        Some("application/x-typescript".to_string()),
        MediaType::Dts,
        None,
      ),
    ];

    for (specifier, maybe_content_type, media_type, maybe_charset) in fixtures {
      let specifier = ModuleSpecifier::resolve_url_or_path(specifier).unwrap();
      assert_eq!(
        map_content_type(&specifier, maybe_content_type),
        (media_type, maybe_charset)
      );
    }
  }

  #[tokio::test]
  async fn test_insert_cached() {
    let (file_fetcher, temp_dir) = setup(CacheSetting::Use, None);
    let local = temp_dir.path().join("a.ts");
    let specifier =
      ModuleSpecifier::resolve_url_or_path(local.as_os_str().to_str().unwrap())
        .unwrap();
    let file = File {
      local,
      maybe_types: None,
      media_type: MediaType::TypeScript,
      source: "some source code".to_string(),
      specifier: specifier.clone(),
    };
    file_fetcher.insert_cached(file.clone());

    let result = file_fetcher
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());
    let result_file = result.unwrap();
    assert_eq!(result_file, file);
  }

  #[tokio::test]
  async fn test_get_cached() {
    let _http_server_guard = test_util::http_server();
    let (file_fetcher, _) = setup(CacheSetting::Use, None);
    let specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4548/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();

    let result = file_fetcher
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());

    let maybe_file = file_fetcher.get_cached(&specifier);
    assert!(maybe_file.is_some());
    let file = maybe_file.unwrap();
    assert_eq!(file.source, "export const redirect = 1;\n");
    assert_eq!(
      file.specifier,
      ModuleSpecifier::resolve_url(
        "http://localhost:4545/cli/tests/subdir/redirects/redirect1.js"
      )
      .unwrap()
    );
  }

  #[test]
  fn test_get_http_cache_location() {
    let (file_fetcher, temp_dir) = setup(CacheSetting::Use, None);
    let expected = temp_dir.path().join("deps");
    let actual = file_fetcher.get_http_cache_location();
    assert_eq!(actual, expected);
  }

  #[tokio::test]
  async fn test_fetch_complex() {
    let _http_server_guard = test_util::http_server();
    let (file_fetcher, temp_dir) = setup(CacheSetting::Use, None);
    let (file_fetcher_01, _) = setup(CacheSetting::Use, Some(temp_dir.clone()));
    let (file_fetcher_02, _) = setup(CacheSetting::Use, Some(temp_dir.clone()));
    let specifier = ModuleSpecifier::resolve_url_or_path(
      "http://localhost:4545/cli/tests/subdir/mod2.ts",
    )
    .unwrap();

    let result = file_fetcher
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());
    let file = result.unwrap();
    assert_eq!(
      file.source,
      "export { printHello } from \"./print_hello.ts\";\n"
    );
    assert_eq!(file.media_type, MediaType::TypeScript);

    let cache_filename = file_fetcher
      .http_cache
      .get_cache_filename(specifier.as_url());
    let mut metadata =
      crate::http_cache::Metadata::read(&cache_filename).unwrap();
    metadata.headers = HashMap::new();
    metadata
      .headers
      .insert("content-type".to_string(), "text/javascript".to_string());
    metadata.write(&cache_filename).unwrap();

    let result = file_fetcher_01
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());
    let file = result.unwrap();
    assert_eq!(
      file.source,
      "export { printHello } from \"./print_hello.ts\";\n"
    );
    // This validates that when using the cached value, because we modified
    // the value above.
    assert_eq!(file.media_type, MediaType::JavaScript);

    let (_, headers) =
      file_fetcher_02.http_cache.get(specifier.as_url()).unwrap();
    assert_eq!(headers.get("content-type").unwrap(), "text/javascript");
    metadata.headers = HashMap::new();
    metadata
      .headers
      .insert("content-type".to_string(), "application/json".to_string());
    metadata.write(&cache_filename).unwrap();

    let result = file_fetcher_02
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());
    let file = result.unwrap();
    assert_eq!(
      file.source,
      "export { printHello } from \"./print_hello.ts\";\n"
    );
    assert_eq!(file.media_type, MediaType::Json);

    // This creates a totally new instance, simulating another Deno process
    // invocation and indicates to "cache bust".
    let location = temp_dir.path().join("deps");
    let file_fetcher = FileFetcher::new(
      HttpCache::new(&location),
      CacheSetting::ReloadAll,
      true,
      None,
    )
    .expect("setup failed");
    let result = file_fetcher
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());
    let file = result.unwrap();
    assert_eq!(
      file.source,
      "export { printHello } from \"./print_hello.ts\";\n"
    );
    assert_eq!(file.media_type, MediaType::TypeScript);
  }

  #[tokio::test]
  async fn test_fetch_uses_cache() {
    let _http_server_guard = test_util::http_server();
    let temp_dir = TempDir::new()
      .expect("could not create temp dir")
      .into_path();
    let location = temp_dir.join("deps");
    let file_fetcher_01 = FileFetcher::new(
      HttpCache::new(&location),
      CacheSetting::Use,
      true,
      None,
    )
    .expect("could not create file fetcher");
    let specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4545/cli/tests/subdir/mismatch_ext.ts",
    )
    .unwrap();
    let cache_filename = file_fetcher_01
      .http_cache
      .get_cache_filename(specifier.as_url());

    let result = file_fetcher_01
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());

    let metadata_filename =
      crate::http_cache::Metadata::filename(&cache_filename);
    let metadata_file =
      fs::File::open(metadata_filename).expect("could not open metadata file");
    let metadata_file_metadata = metadata_file.metadata().unwrap();
    let metadata_file_modified_01 = metadata_file_metadata.modified().unwrap();

    let file_fetcher_02 = FileFetcher::new(
      HttpCache::new(&location),
      CacheSetting::Use,
      true,
      None,
    )
    .expect("could not create file fetcher");
    let result = file_fetcher_02
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());

    let metadata_filename =
      crate::http_cache::Metadata::filename(&cache_filename);
    let metadata_file =
      fs::File::open(metadata_filename).expect("could not open metadata file");
    let metadata_file_metadata = metadata_file.metadata().unwrap();
    let metadata_file_modified_02 = metadata_file_metadata.modified().unwrap();

    assert_eq!(metadata_file_modified_01, metadata_file_modified_02);
    // because we converted to a "fixed" directory, we need to cleanup after
    // ourselves.
    let _ = fs::remove_dir_all(temp_dir);
  }

  #[tokio::test]
  async fn test_fetch_redirected() {
    let _http_server_guard = test_util::http_server();
    let (file_fetcher, _) = setup(CacheSetting::Use, None);
    let specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4546/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let cached_filename = file_fetcher
      .http_cache
      .get_cache_filename(specifier.as_url());
    let redirected_specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4545/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let redirected_cached_filename = file_fetcher
      .http_cache
      .get_cache_filename(redirected_specifier.as_url());

    let result = file_fetcher
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());
    let file = result.unwrap();
    assert_eq!(file.specifier, redirected_specifier);

    assert_eq!(
      fs::read_to_string(cached_filename).unwrap(),
      "",
      "redirected files should have empty cached contents"
    );
    let (_, headers) = file_fetcher
      .http_cache
      .get(specifier.as_url())
      .expect("could not get file");
    assert_eq!(
      headers.get("location").unwrap(),
      "http://localhost:4545/cli/tests/subdir/redirects/redirect1.js"
    );

    assert_eq!(
      fs::read_to_string(redirected_cached_filename).unwrap(),
      "export const redirect = 1;\n"
    );
    let (_, headers) = file_fetcher
      .http_cache
      .get(redirected_specifier.as_url())
      .expect("could not get file");
    assert!(headers.get("location").is_none());
  }

  #[tokio::test]
  async fn test_fetch_multiple_redirects() {
    let _http_server_guard = test_util::http_server();
    let (file_fetcher, _) = setup(CacheSetting::Use, None);
    let specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4548/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let cached_filename = file_fetcher
      .http_cache
      .get_cache_filename(specifier.as_url());
    let redirected_01_specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4546/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let redirected_01_cached_filename = file_fetcher
      .http_cache
      .get_cache_filename(redirected_01_specifier.as_url());
    let redirected_02_specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4545/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let redirected_02_cached_filename = file_fetcher
      .http_cache
      .get_cache_filename(redirected_02_specifier.as_url());

    let result = file_fetcher
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());
    let file = result.unwrap();
    assert_eq!(file.specifier, redirected_02_specifier);

    assert_eq!(
      fs::read_to_string(cached_filename).unwrap(),
      "",
      "redirected files should have empty cached contents"
    );
    let (_, headers) = file_fetcher
      .http_cache
      .get(specifier.as_url())
      .expect("could not get file");
    assert_eq!(
      headers.get("location").unwrap(),
      "http://localhost:4546/cli/tests/subdir/redirects/redirect1.js"
    );

    assert_eq!(
      fs::read_to_string(redirected_01_cached_filename).unwrap(),
      "",
      "redirected files should have empty cached contents"
    );
    let (_, headers) = file_fetcher
      .http_cache
      .get(redirected_01_specifier.as_url())
      .expect("could not get file");
    assert_eq!(
      headers.get("location").unwrap(),
      "http://localhost:4545/cli/tests/subdir/redirects/redirect1.js"
    );

    assert_eq!(
      fs::read_to_string(redirected_02_cached_filename).unwrap(),
      "export const redirect = 1;\n"
    );
    let (_, headers) = file_fetcher
      .http_cache
      .get(redirected_02_specifier.as_url())
      .expect("could not get file");
    assert!(headers.get("location").is_none());
  }

  #[tokio::test]
  async fn test_fetch_uses_cache_with_redirects() {
    let _http_server_guard = test_util::http_server();
    let temp_dir = TempDir::new()
      .expect("could not create temp dir")
      .into_path();
    let location = temp_dir.join("deps");
    let file_fetcher_01 = FileFetcher::new(
      HttpCache::new(&location),
      CacheSetting::Use,
      true,
      None,
    )
    .expect("could not create file fetcher");
    let specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4548/cli/tests/subdir/mismatch_ext.ts",
    )
    .unwrap();
    let redirected_specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4546/cli/tests/subdir/mismatch_ext.ts",
    )
    .unwrap();
    let redirected_cache_filename = file_fetcher_01
      .http_cache
      .get_cache_filename(redirected_specifier.as_url());

    let result = file_fetcher_01
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());

    let metadata_filename =
      crate::http_cache::Metadata::filename(&redirected_cache_filename);
    let metadata_file =
      fs::File::open(metadata_filename).expect("could not open metadata file");
    let metadata_file_metadata = metadata_file.metadata().unwrap();
    let metadata_file_modified_01 = metadata_file_metadata.modified().unwrap();

    let file_fetcher_02 = FileFetcher::new(
      HttpCache::new(&location),
      CacheSetting::Use,
      true,
      None,
    )
    .expect("could not create file fetcher");
    let result = file_fetcher_02
      .fetch(&redirected_specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());

    let metadata_filename =
      crate::http_cache::Metadata::filename(&redirected_cache_filename);
    let metadata_file =
      fs::File::open(metadata_filename).expect("could not open metadata file");
    let metadata_file_metadata = metadata_file.metadata().unwrap();
    let metadata_file_modified_02 = metadata_file_metadata.modified().unwrap();

    assert_eq!(metadata_file_modified_01, metadata_file_modified_02);
    // because we converted to a "fixed" directory, we need to cleanup after
    // ourselves.
    let _ = fs::remove_dir_all(temp_dir);
  }

  #[tokio::test]
  async fn test_fetcher_limits_redirects() {
    let _http_server_guard = test_util::http_server();
    let (file_fetcher, _) = setup(CacheSetting::Use, None);
    let specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4548/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();

    let result = file_fetcher
      .fetch_remote(&specifier, &Permissions::allow_all(), 2)
      .await;
    assert!(result.is_ok());

    let result = file_fetcher
      .fetch_remote(&specifier, &Permissions::allow_all(), 1)
      .await;
    assert!(result.is_err());

    let result = file_fetcher.fetch_cached(&specifier, 2);
    assert!(result.is_ok());

    let result = file_fetcher.fetch_cached(&specifier, 1);
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn test_fetch_same_host_redirect() {
    let _http_server_guard = test_util::http_server();
    let (file_fetcher, _) = setup(CacheSetting::Use, None);
    let specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4550/REDIRECT/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let cached_filename = file_fetcher
      .http_cache
      .get_cache_filename(specifier.as_url());
    let redirected_specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4550/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let redirected_cached_filename = file_fetcher
      .http_cache
      .get_cache_filename(redirected_specifier.as_url());

    let result = file_fetcher
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());
    let file = result.unwrap();
    assert_eq!(file.specifier, redirected_specifier);

    assert_eq!(
      fs::read_to_string(cached_filename).unwrap(),
      "",
      "redirected files should have empty cached contents"
    );
    let (_, headers) = file_fetcher
      .http_cache
      .get(specifier.as_url())
      .expect("could not get file");
    assert_eq!(
      headers.get("location").unwrap(),
      "/cli/tests/subdir/redirects/redirect1.js"
    );

    assert_eq!(
      fs::read_to_string(redirected_cached_filename).unwrap(),
      "export const redirect = 1;\n"
    );
    let (_, headers) = file_fetcher
      .http_cache
      .get(redirected_specifier.as_url())
      .expect("could not get file");
    assert!(headers.get("location").is_none());
  }

  #[tokio::test]
  async fn test_fetch_no_remote() {
    let _http_server_guard = test_util::http_server();
    let temp_dir = TempDir::new().expect("could not create temp dir");
    let location = temp_dir.path().join("deps");
    let file_fetcher = FileFetcher::new(
      HttpCache::new(&location),
      CacheSetting::Use,
      false,
      None,
    )
    .expect("could not create file fetcher");
    let specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4545/cli/tests/002_hello.ts",
    )
    .unwrap();

    let result = file_fetcher
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(get_custom_error_class(&err), Some("NoRemote"));
    assert_eq!(err.to_string(), "A remote specifier was requested: \"http://localhost:4545/cli/tests/002_hello.ts\", but --no-remote is specified.");
  }

  #[tokio::test]
  async fn test_fetch_cache_only() {
    let _http_server_guard = test_util::http_server();
    let temp_dir = TempDir::new()
      .expect("could not create temp dir")
      .into_path();
    let location = temp_dir.join("deps");
    let file_fetcher_01 = FileFetcher::new(
      HttpCache::new(&location),
      CacheSetting::Only,
      true,
      None,
    )
    .expect("could not create file fetcher");
    let file_fetcher_02 = FileFetcher::new(
      HttpCache::new(&location),
      CacheSetting::Use,
      true,
      None,
    )
    .expect("could not create file fetcher");
    let specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4545/cli/tests/002_hello.ts",
    )
    .unwrap();

    let result = file_fetcher_01
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(get_custom_error_class(&err), Some("NotFound"));
    assert_eq!(err.to_string(), "Specifier not found in cache: \"http://localhost:4545/cli/tests/002_hello.ts\", --cached-only is specified.");

    let result = file_fetcher_02
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());

    let result = file_fetcher_01
      .fetch(&specifier, &Permissions::allow_all())
      .await;
    assert!(result.is_ok());

    // because we converted to a "fixed" directory, we need to cleanup after
    // ourselves.
    let _ = fs::remove_dir_all(temp_dir);
  }

  #[tokio::test]
  async fn test_fetch_local_utf_16be() {
    let expected = String::from_utf8(
      b"\xEF\xBB\xBFconsole.log(\"Hello World\");\x0A".to_vec(),
    )
    .unwrap();
    test_fetch_local_encoded("utf-16be", expected).await;
  }

  #[tokio::test]
  async fn test_fetch_local_utf_16le() {
    let expected = String::from_utf8(
      b"\xEF\xBB\xBFconsole.log(\"Hello World\");\x0A".to_vec(),
    )
    .unwrap();
    test_fetch_local_encoded("utf-16le", expected).await;
  }

  #[tokio::test]
  async fn test_fetch_local_utf8_with_bom() {
    let expected = String::from_utf8(
      b"\xEF\xBB\xBFconsole.log(\"Hello World\");\x0A".to_vec(),
    )
    .unwrap();
    test_fetch_local_encoded("utf-8", expected).await;
  }

  #[tokio::test]
  async fn test_fetch_remote_with_types() {
    let specifier = ModuleSpecifier::resolve_url_or_path(
      "http://127.0.0.1:4545/xTypeScriptTypes.js",
    )
    .unwrap();
    let (file, _) = test_fetch_remote(&specifier).await;
    assert_eq!(
      file.maybe_types,
      Some("./xTypeScriptTypes.d.ts".to_string())
    );
  }

  #[tokio::test]
  async fn test_fetch_remote_utf16_le() {
    let expected =
      std::str::from_utf8(b"\xEF\xBB\xBFconsole.log(\"Hello World\");\x0A")
        .unwrap();
    test_fetch_remote_encoded("utf-16le.ts", "utf-16le", expected).await;
  }

  #[tokio::test]
  async fn test_fetch_remote_utf16_be() {
    let expected =
      std::str::from_utf8(b"\xEF\xBB\xBFconsole.log(\"Hello World\");\x0A")
        .unwrap();
    test_fetch_remote_encoded("utf-16be.ts", "utf-16be", expected).await;
  }

  #[tokio::test]
  async fn test_fetch_remote_window_1255() {
    let expected = "console.log(\"\u{5E9}\u{5DC}\u{5D5}\u{5DD} \
                   \u{5E2}\u{5D5}\u{5DC}\u{5DD}\");\u{A}";
    test_fetch_remote_encoded("windows-1255", "windows-1255", expected).await;
  }
}

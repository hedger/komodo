use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use formatting::format_serror;
use komodo_client::entities::{
  FileContents,
  stack::{Stack, StackFileDependency, StackRemoteFileContents},
  update::Log,
};
use periphery_client::api::compose::ComposeUpResponse;
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::docker::docker_login;

/// Expand a single file dependency, potentially with glob pattern.
/// Returns a vector of (full_path, file_dependency) tuples.
fn expand_file_dependency(
  run_directory: &Path,
  file: StackFileDependency,
) -> Vec<(PathBuf, StackFileDependency)> {
  if !file.glob {
    // Simple case: not a glob, just return the single file
    let full_path = run_directory
      .join(&file.path)
      .components()
      .collect::<PathBuf>();
    return vec![(full_path, file)];
  }

  // Glob case: expand the pattern
  let pattern = run_directory
    .join(&file.path)
    .to_string_lossy()
    .to_string();

  let Ok(entries) = glob::glob(&pattern) else {
    // If glob pattern is invalid, treat as missing file
    let full_path = run_directory
      .join(&file.path)
      .components()
      .collect::<PathBuf>();
    return vec![(full_path, file)];
  };

  let mut results = Vec::new();
  for entry in entries.flatten() {
    // Only include files, not directories
    // Use metadata to check if it's a file to handle symlinks properly
    if let Ok(metadata) = entry.metadata() {
      if metadata.is_file() {
        // Get the relative path from run_directory
        let relative_path = entry
          .strip_prefix(run_directory)
          .unwrap_or(&entry)
          .to_string_lossy()
          .to_string()
          // Normalize path separators to forward slashes for consistency
          .replace('\\', "/");

        let expanded_file = StackFileDependency {
          path: relative_path,
          glob: false, // Individual expanded files are not globs
          use_hash: file.use_hash,
          services: file.services.clone(),
          requires: file.requires,
        };

        results.push((entry, expanded_file));
      }
    }
  }

  results
}

/// Calculate SHA256 hash of file contents
async fn calculate_file_hash(path: &Path) -> anyhow::Result<String> {
  let bytes = fs::read(path)
    .await
    .with_context(|| format!("Failed to read file for hashing: {path:?}"))?;
  let mut hasher = Sha256::new();
  hasher.update(&bytes);
  let hash_bytes = hasher.finalize();
  Ok(hex::encode(hash_bytes))
}

pub async fn validate_files(
  stack: &Stack,
  run_directory: &Path,
  res: &mut ComposeUpResponse,
) {
  // Expand all file dependencies, including globs
  let mut file_paths = Vec::new();
  for file in stack.all_file_dependencies() {
    file_paths.extend(expand_file_dependency(run_directory, file));
  }

  // First validate no missing files
  for (full_path, file) in &file_paths {
    if !full_path.exists() {
      res.missing_files.push(file.path.clone());
    }
  }
  if !res.missing_files.is_empty() {
    res.logs.push(Log::error(
      "Validate Files",
      format_serror(
        &anyhow!(
          "Missing files: {}", res.missing_files.join(", ")
        )
        .context("Ensure the run_directory and all file paths are correct.")
        .context("A file doesn't exist after writing stack.")
        .into(),
      ),
    ));
    return;
  }

  // Process each file
  for (full_path, file) in file_paths {
    if file.use_hash {
      // Use hash instead of contents for large/binary files
      match calculate_file_hash(&full_path).await {
        Ok(hash) => {
          res.file_contents.push(StackRemoteFileContents {
            path: file.path,
            contents: String::new(), // Empty contents when using hash
            hash: Some(hash),
            services: file.services,
            requires: file.requires,
          });
        }
        Err(e) => {
          let error = format_serror(&e.into());
          res
            .logs
            .push(Log::error("Calculate File Hash", error.clone()));
          res.remote_errors.push(FileContents {
            path: file.path,
            contents: error,
          });
          return;
        }
      }
    } else {
      // Use file contents (default behavior)
      let file_contents =
        match fs::read_to_string(&full_path).await.with_context(|| {
          format!("Failed to read file contents at {full_path:?}")
        }) {
          Ok(res) => res,
          Err(e) => {
            let error = format_serror(&e.into());
            res
              .logs
              .push(Log::error("Read Compose File", error.clone()));
            // This should only happen for repo stacks, ie remote error
            res.remote_errors.push(FileContents {
              path: file.path,
              contents: error,
            });
            return;
          }
        };
      res.file_contents.push(StackRemoteFileContents {
        path: file.path,
        contents: file_contents,
        hash: None,
        services: file.services,
        requires: file.requires,
      });
    }
  }
}

pub async fn maybe_login_registry(
  stack: &Stack,
  registry_token: Option<String>,
  logs: &mut Vec<Log>,
) {
  if !stack.config.registry_provider.is_empty()
    && !stack.config.registry_account.is_empty()
    && let Err(e) = docker_login(
      &stack.config.registry_provider,
      &stack.config.registry_account,
      registry_token.as_deref(),
    )
    .await
    .with_context(|| {
      format!(
        "Domain: '{}' | Account: '{}'",
        stack.config.registry_provider, stack.config.registry_account
      )
    })
    .context("Failed to login to image registry")
  {
    logs.push(Log::error(
      "Login to Registry",
      format_serror(&e.into()),
    ));
  }
}

use std::{fs, path::PathBuf};

use anyhow::Context;
use formatting::format_serror;
use komodo_client::entities::{
  FileContents, RepoExecutionArgs,
  repo::Repo,
  stack::{
    Stack, StackRemoteFileContents,
    calculate_file_hash_sync, expand_file_dependency,
  },
  update::Log,
};

use crate::{config::core_config, helpers::git_token};

#[derive(Default)]
pub struct RemoteComposeContents {
  pub successful: Vec<StackRemoteFileContents>,
  pub errored: Vec<FileContents>,
  pub hash: Option<String>,
  pub message: Option<String>,
  // pub logs: Vec<Log>,
}

/// Returns Result<(read paths, error paths, logs, short hash, commit message)>
pub async fn get_repo_compose_contents(
  stack: &Stack,
  repo: Option<&Repo>,
  // Collect any files which are missing in the repo.
  mut missing_files: Option<&mut Vec<String>>,
) -> anyhow::Result<RemoteComposeContents> {
  let clone_args: RepoExecutionArgs =
    repo.map(Into::into).unwrap_or(stack.into());
  let (repo_path, logs, hash, message) =
    ensure_remote_repo(clone_args)
      .await
      .context("Failed to clone stack repo")?;

  // Ensure clone / pull successful,
  // propogate error log -> 'errored' and return.
  if let Some(failure) = logs.iter().find(|log| !log.success) {
    return Ok(RemoteComposeContents {
      errored: vec![FileContents {
        path: format!("Failed at: {}", failure.stage),
        contents: failure.combined(),
        hash: None,
      }],
      ..Default::default()
    });
  }

  let run_directory = repo_path.join(&stack.config.run_directory);
  // This will remove any intermediate '/./' which can be a problem for some OS.
  let run_directory = run_directory.components().collect::<PathBuf>();

  let mut successful = Vec::new();
  let mut errored = Vec::new();

  // Expand all file dependencies, including globs
  let mut expanded_paths = Vec::new();
  for file in stack.all_file_dependencies() {
    expanded_paths.extend(expand_file_dependency(&run_directory, file));
  }

  for (file_path, file) in expanded_paths {
    if !file_path.exists() {
      if let Some(missing_files) = &mut missing_files {
        missing_files.push(file.path.clone());
      }
    }

    if file.use_hash {
      // Use hash instead of contents for large/binary files
      match calculate_file_hash_sync(&file_path) {
        Ok(hash) => successful.push(StackRemoteFileContents {
          path: file.path,
          contents: String::new(), // Empty contents when using hash
          hash: Some(hash),
          services: file.services,
          requires: file.requires,
        }),
        Err(e) => errored.push(FileContents {
          path: file.path,
          contents: format_serror(&e.into()),
          hash: None,
        }),
      }
    } else {
      // Use file contents (default behavior)
      // If file does not exist, will show up in err case so the log is handled
      match fs::read_to_string(&file_path).with_context(|| {
        format!("Failed to read file contents from {file_path:?}")
      }) {
        Ok(contents) => successful.push(StackRemoteFileContents {
          path: file.path,
          contents,
          hash: None,
          services: file.services,
          requires: file.requires,
        }),
        Err(e) => errored.push(FileContents {
          path: file.path,
          contents: format_serror(&e.into()),
          hash: None,
        }),
      }
    }
  }

  Ok(RemoteComposeContents {
    successful,
    errored,
    hash,
    message,
  })
}

/// Returns (destination, logs, hash, message)
pub async fn ensure_remote_repo(
  mut clone_args: RepoExecutionArgs,
) -> anyhow::Result<(PathBuf, Vec<Log>, Option<String>, Option<String>)>
{
  let config = core_config();

  let access_token = if let Some(username) = &clone_args.account {
    git_token(&clone_args.provider, username, |https| {
        clone_args.https = https
      })
      .await
      .with_context(
        || format!("Failed to get git token in call to db. Stopping run. | {} | {username}", clone_args.provider),
      )?
  } else {
    None
  };

  let repo_path =
    clone_args.unique_path(&core_config().repo_directory)?;
  clone_args.destination = Some(repo_path.display().to_string());

  git::pull_or_clone(clone_args, &config.repo_directory, access_token)
    .await
    .context("Failed to clone stack repo")
    .map(|(res, _)| {
      (repo_path, res.logs, res.commit_hash, res.commit_message)
    })
}

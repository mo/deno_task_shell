// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::bail;
use anyhow::Result;
use futures::FutureExt;

use crate::commands::cd_command;
use crate::commands::cp_command;
use crate::commands::exit_command;
use crate::commands::mkdir_command;
use crate::commands::mv_command;
use crate::commands::rm_command;
use crate::commands::sleep_command;
use crate::parser::Command;
use crate::parser::Sequence;
use crate::parser::SequentialList;
use crate::parser::StringOrWord;
use crate::parser::StringPart;
use crate::shell_types::pipe;
use crate::shell_types::EnvChange;
use crate::shell_types::ExecuteResult;
use crate::shell_types::FutureExecuteResult;
use crate::shell_types::ShellPipeReader;
use crate::shell_types::ShellPipeWriter;
use crate::shell_types::ShellState;

pub async fn execute(
  list: SequentialList,
  env_vars: HashMap<String, String>,
  cwd: &Path,
) -> i32 {
  execute_with_pipes(
    list,
    env_vars,
    cwd,
    ShellPipeReader::stdin(),
    ShellPipeWriter::stdout(),
    ShellPipeWriter::stderr(),
  )
  .await
}

pub(crate) async fn execute_with_pipes(
  list: SequentialList,
  env_vars: HashMap<String, String>,
  cwd: &Path,
  stdin: ShellPipeReader,
  stdout: ShellPipeWriter,
  stderr: ShellPipeWriter,
) -> i32 {
  assert!(cwd.is_absolute());
  let state = ShellState::new(env_vars, cwd);

  // spawn a sequential list and pipe its output to the environment
  let result = execute_sequential_list(
    list,
    state,
    stdin,
    stdout,
    stderr,
    AsyncCommandBehavior::Wait,
  )
  .await;

  match result {
    ExecuteResult::Exit(code, _) => code,
    ExecuteResult::Continue(exit_code, _, _) => exit_code,
  }
}

#[derive(Debug, PartialEq)]
enum AsyncCommandBehavior {
  Wait,
  Yield,
}

fn execute_sequential_list(
  list: SequentialList,
  mut state: ShellState,
  stdin: ShellPipeReader,
  stdout: ShellPipeWriter,
  stderr: ShellPipeWriter,
  async_command_behavior: AsyncCommandBehavior,
) -> FutureExecuteResult {
  async move {
    let mut final_exit_code = 0;
    let mut final_changes = Vec::new();
    let mut async_handles = Vec::new();
    for item in list.items {
      if item.is_async {
        let state = state.clone();
        let stdin = stdin.clone();
        let stdout = stdout.clone();
        let stderr = stderr.clone();
        async_handles.push(tokio::task::spawn(async move {
          let result =
            execute_sequence(item.sequence, state, stdin, stdout, stderr).await;
          futures::future::join_all(result.into_handles()).await;
        }));
      } else {
        let result = execute_sequence(
          item.sequence,
          state.clone(),
          stdin.clone(),
          stdout.clone(),
          stderr.clone(),
        )
        .await;
        match result {
          ExecuteResult::Exit(_, _) => return result,
          ExecuteResult::Continue(exit_code, changes, handles) => {
            state.apply_changes(&changes);
            final_changes.extend(changes);
            async_handles.extend(handles);
            // use the final sequential item's exit code
            final_exit_code = exit_code;
          }
        }
      }
    }

    // wait for async commands to complete
    if async_command_behavior == AsyncCommandBehavior::Wait {
      futures::future::join_all(async_handles.drain(..)).await;
    }

    ExecuteResult::Continue(final_exit_code, final_changes, async_handles)
  }
  .boxed()
}

fn execute_sequence(
  sequence: Sequence,
  mut state: ShellState,
  stdin: ShellPipeReader,
  stdout: ShellPipeWriter,
  stderr: ShellPipeWriter,
) -> FutureExecuteResult {
  // requires boxed async because of recursive async
  async move {
    match sequence {
      Sequence::EnvVar(var) => ExecuteResult::Continue(
        0,
        vec![EnvChange::SetEnvVar(
          var.name,
          evaluate_string_or_word(var.value, &state, stdin, stderr).await,
        )],
        Vec::new(),
      ),
      Sequence::ShellVar(var) => ExecuteResult::Continue(
        0,
        vec![EnvChange::SetShellVar(
          var.name,
          evaluate_string_or_word(var.value, &state, stdin, stderr).await,
        )],
        Vec::new(),
      ),
      Sequence::Command(command) => {
        execute_command(command, state, stdin, stdout, stderr).await
      }
      Sequence::BooleanList(list) => {
        let mut changes = vec![];
        let first_result = execute_sequence(
          list.current,
          state.clone(),
          stdin.clone(),
          stdout.clone(),
          stderr.clone(),
        )
        .await;
        let (exit_code, mut async_handles) = match first_result {
          ExecuteResult::Exit(_, _) => return first_result,
          ExecuteResult::Continue(exit_code, sub_changes, async_handles) => {
            state.apply_changes(&sub_changes);
            changes.extend(sub_changes);
            (exit_code, async_handles)
          }
        };

        let next = if list.op.moves_next_for_exit_code(exit_code) {
          Some(list.next)
        } else {
          let mut next = list.next;
          loop {
            // boolean lists always move right on the tree
            match next {
              Sequence::BooleanList(list) => {
                if list.op.moves_next_for_exit_code(exit_code) {
                  break Some(list.next);
                }
                next = list.next;
              }
              _ => break None,
            }
          }
        };
        if let Some(next) = next {
          let next_result =
            execute_sequence(next, state, stdin, stdout, stderr).await;
          match next_result {
            ExecuteResult::Exit(code, sub_handles) => {
              async_handles.extend(sub_handles);
              ExecuteResult::Exit(code, async_handles)
            }
            ExecuteResult::Continue(exit_code, sub_changes, sub_handles) => {
              changes.extend(sub_changes);
              async_handles.extend(sub_handles);
              ExecuteResult::Continue(exit_code, changes, async_handles)
            }
          }
        } else {
          ExecuteResult::Continue(exit_code, changes, async_handles)
        }
      }
      Sequence::Pipeline(pipeline) => {
        let sequences = pipeline.into_vec();
        let mut wait_tasks = vec![];
        let mut last_input = Some(stdin);
        for sequence in sequences.into_iter() {
          let (stdin, stdout) = pipe();
          wait_tasks.push(execute_sequence(
            sequence,
            state.clone(),
            last_input.take().unwrap(),
            stdout,
            stderr.clone(),
          ));
          last_input = Some(stdin);
        }
        let output_handle = tokio::task::spawn_blocking(|| {
          last_input.unwrap().pipe_to_sender(stdout).unwrap();
        });
        let mut results = futures::future::join_all(wait_tasks).await;
        output_handle.await.unwrap();
        let last_result = results.pop().unwrap();
        let all_handles = results.into_iter().flat_map(|r| r.into_handles());
        match last_result {
          ExecuteResult::Exit(code, mut handles) => {
            handles.extend(all_handles);
            ExecuteResult::Continue(code, Vec::new(), handles)
          }
          ExecuteResult::Continue(code, _, mut handles) => {
            handles.extend(all_handles);
            ExecuteResult::Continue(code, Vec::new(), handles)
          }
        }
      }
      Sequence::Subshell(list) => {
        let result = execute_sequential_list(
          *list,
          state.clone(),
          stdin,
          stdout,
          stderr,
          // yield async commands to the parent
          AsyncCommandBehavior::Yield,
        )
        .await;

        // sub shells do not cause an exit
        match result {
          ExecuteResult::Exit(code, handles) => {
            ExecuteResult::Continue(code, Vec::new(), handles)
          }
          ExecuteResult::Continue(_, _, _) => result,
        }
      }
    }
  }
  .boxed()
}

async fn execute_command(
  command: Command,
  state: ShellState,
  stdin: ShellPipeReader,
  stdout: ShellPipeWriter,
  mut stderr: ShellPipeWriter,
) -> ExecuteResult {
  let mut args =
    evaluate_args(command.args, &state, stdin.clone(), stderr.clone()).await;
  let command_name = if args.is_empty() {
    String::new()
  } else {
    args.remove(0)
  };
  if command_name == "cd" {
    let cwd = state.cwd().clone();
    cd_command(&cwd, args, stderr)
  } else if command_name == "exit" {
    exit_command(args, stderr)
  } else if command_name == "pwd" {
    // ignores additional arguments
    ExecuteResult::with_stdout_text(
      stdout,
      format!("{}\n", state.cwd().display()),
    )
  } else if command_name == "echo" {
    ExecuteResult::with_stdout_text(stdout, format!("{}\n", args.join(" ")))
  } else if command_name == "true" {
    // ignores additional arguments
    ExecuteResult::from_exit_code(0)
  } else if command_name == "false" {
    // ignores additional arguments
    ExecuteResult::from_exit_code(1)
  } else if command_name == "cp" {
    let cwd = state.cwd().clone();
    cp_command(&cwd, args, stderr).await
  } else if command_name == "mkdir" {
    let cwd = state.cwd().clone();
    mkdir_command(&cwd, args, stderr).await
  } else if command_name == "mv" {
    let cwd = state.cwd().clone();
    mv_command(&cwd, args, stderr).await
  } else if command_name == "rm" {
    let cwd = state.cwd().clone();
    rm_command(&cwd, args, stderr).await
  } else if command_name == "sleep" {
    sleep_command(args, stderr).await
  } else {
    let mut state = state.clone();
    for env_var in command.env_vars {
      state.apply_env_var(
        &env_var.name,
        &evaluate_string_or_word(
          env_var.value,
          &state,
          stdin.clone(),
          stderr.clone(),
        )
        .await,
      );
    }

    let command_path = match resolve_command_path(&command_name, &state).await {
      Ok(command_path) => command_path,
      Err(err) => {
        stderr.write_line(&err.to_string()).unwrap();
        return ExecuteResult::Continue(1, Vec::new(), Vec::new());
      }
    };
    let mut sub_command = tokio::process::Command::new(&command_path);
    let child = sub_command
      .current_dir(state.cwd())
      .args(&args)
      .env_clear()
      .envs(state.env_vars())
      .stdout(stdout.into_raw())
      .stdin(stdin.into_raw())
      .stderr(Stdio::inherit())
      .spawn();

    let mut child = match child {
      Ok(child) => child,
      Err(err) => {
        stderr
          .write_line(&format!("Error launching '{}': {}", command_name, err))
          .unwrap();
        return ExecuteResult::Continue(1, Vec::new(), Vec::new());
      }
    };

    // avoid deadlock since this is holding onto the pipes
    drop(sub_command);

    match child.wait().await {
      Ok(status) => ExecuteResult::Continue(
        status.code().unwrap_or(1),
        Vec::new(),
        Vec::new(),
      ),
      Err(err) => {
        stderr.write_line(&format!("{}", err)).unwrap();
        ExecuteResult::Continue(1, Vec::new(), Vec::new())
      }
    }
  }
}

async fn resolve_command_path(
  command_name: &str,
  state: &ShellState,
) -> Result<PathBuf> {
  if command_name.is_empty() {
    bail!("command name was empty");
  }

  // check for absolute
  if PathBuf::from(command_name).is_absolute() {
    return Ok(PathBuf::from(command_name));
  }

  // then relative
  if command_name.contains('/')
    || (cfg!(windows) && command_name.contains('\\'))
  {
    return Ok(state.cwd().join(&command_name));
  }

  // now search based on the current environment state
  let mut search_dirs = vec![state.cwd().clone()];
  if let Some(path) = state.get_var("PATH") {
    for folder in path.split(if cfg!(windows) { ';' } else { ':' }) {
      search_dirs.push(PathBuf::from(folder));
    }
  }
  let path_exts = if cfg!(windows) {
    let path_ext = state
      .get_var("PATHEXT")
      .map(|s| s.as_str())
      .unwrap_or(".EXE;.CMD;.BAT;.COM");
    let command_exts = path_ext
      .split(';')
      .map(|s| s.to_string().to_uppercase())
      .collect::<Vec<_>>();
    if command_exts.iter().any(|ext| command_name.ends_with(ext)) {
      None // use the command name as-is
    } else {
      Some(command_exts)
    }
  } else {
    None
  };

  for search_dir in search_dirs {
    let paths = if let Some(path_exts) = &path_exts {
      let mut paths = Vec::new();
      for path_ext in path_exts {
        paths.push(search_dir.join(format!("{}{}", command_name, path_ext)))
      }
      paths
    } else {
      vec![search_dir.join(command_name)]
    };
    for path in paths {
      if let Ok(metadata) = tokio::fs::metadata(&path).await {
        if metadata.is_file() {
          return Ok(path);
        }
      }
    }
  }

  bail!("{}: command not found", command_name)
}

async fn evaluate_args(
  args: Vec<StringOrWord>,
  state: &ShellState,
  stdin: ShellPipeReader,
  stderr: ShellPipeWriter,
) -> Vec<String> {
  let mut result = Vec::new();
  for arg in args {
    match arg {
      StringOrWord::Word(parts) => {
        // todo(dsherret): maybe we should have this work like sh and I believe
        // reparse then continually re-evaluate until there's only strings left.
        let text =
          evaluate_string_parts(parts, state, stdin.clone(), stderr.clone())
            .await;
        for part in text.split(' ') {
          let part = part.trim();
          if !part.is_empty() {
            result.push(part.to_string());
          }
        }
      }
      StringOrWord::String(parts) => {
        result.push(
          evaluate_string_parts(parts, state, stdin.clone(), stderr.clone())
            .await,
        );
      }
    }
  }
  result
}

async fn evaluate_string_or_word(
  string_or_word: StringOrWord,
  state: &ShellState,
  stdin: ShellPipeReader,
  stderr: ShellPipeWriter,
) -> String {
  evaluate_string_parts(string_or_word.into_parts(), state, stdin, stderr).await
}

async fn evaluate_string_parts(
  parts: Vec<StringPart>,
  state: &ShellState,
  stdin: ShellPipeReader,
  stderr: ShellPipeWriter,
) -> String {
  let mut final_text = String::new();
  for part in parts {
    match part {
      StringPart::Text(text) => final_text.push_str(&text),
      StringPart::Variable(name) => {
        if let Some(value) = state.get_var(&name) {
          final_text.push_str(value);
        }
      }
      StringPart::Command(list) => final_text.push_str(
        &evaluate_command_substitution(
          list,
          state,
          stdin.clone(),
          stderr.clone(),
        )
        .await,
      ),
    }
  }
  final_text
}

async fn evaluate_command_substitution(
  list: SequentialList,
  state: &ShellState,
  stdin: ShellPipeReader,
  stderr: ShellPipeWriter,
) -> String {
  let text = execute_with_stdout_as_text(|shell_stdout_writer| {
    execute_sequential_list(
      list,
      state.clone(),
      stdin,
      shell_stdout_writer,
      stderr,
      AsyncCommandBehavior::Wait,
    )
  })
  .await;

  // Remove the trailing newline and then replace inner newlines with a space
  // This seems to be what sh does, but I'm not entirely sure:
  //
  // > echo $(echo 1 && echo -e "\n2\n")
  // 1 2
  text
    .strip_suffix("\r\n")
    .or_else(|| text.strip_suffix('\n'))
    .unwrap_or(&text)
    .replace("\r\n", " ")
    .replace('\n', " ")
}

async fn execute_with_stdout_as_text(
  execute: impl FnOnce(ShellPipeWriter) -> FutureExecuteResult,
) -> String {
  let (shell_stdout_reader, shell_stdout_writer) = pipe();
  let spawned_output = execute(shell_stdout_writer);
  let output_handle = tokio::task::spawn_blocking(move || {
    let mut final_data = Vec::new();
    shell_stdout_reader.write_all(&mut final_data).unwrap();
    final_data
  });
  let _ = spawned_output.await;
  let data = output_handle.await.unwrap();
  String::from_utf8_lossy(&data).to_string()
}
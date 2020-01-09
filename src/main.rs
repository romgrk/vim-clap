mod icon;

use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::SystemTime;

use anyhow::Result;
use extracted_fzy::match_and_score_with_positions;
use fuzzy_matcher::skim::fuzzy_indices;
use rayon::prelude::*;
use serde_json::json;
use structopt::clap::arg_enum;
use structopt::StructOpt;

use icon::{prepend_icon, DEFAULT_ICONIZED};

arg_enum! {
    #[derive(Debug)]
    enum Algo {
        Skim,
        Fzy,
    }
}

#[derive(StructOpt, Debug)]
enum Cmd {
    /// Fuzzy filter the input.
    #[structopt(name = "filter")]
    Filter {
        /// Initial query string
        #[structopt(index = 1, short, long)]
        query: String,

        /// Filter algorithm
        #[structopt(short, long, possible_values = &Algo::variants(), case_insensitive = true)]
        algo: Option<Algo>,

        /// Read input from a file instead of stdin, only absolute file path is supported.
        #[structopt(long = "input", parse(from_os_str))]
        input: Option<PathBuf>,
    },
    /// Execute the command.
    #[structopt(name = "exec")]
    Exec {
        /// Specify the system command to run.
        #[structopt(index = 1, short, long)]
        cmd: String,

        /// Specify the output file path when the output of command exceeds the threshold.
        #[structopt(long = "output")]
        output: Option<String>,

        /// Specify the threshold for writing the output of command to a tempfile.
        #[structopt(long = "output-threshold", default_value = "100000")]
        output_threshold: usize,

        /// Specify the working directory of CMD
        #[structopt(long = "cmd-dir", parse(from_os_str))]
        cmd_dir: Option<PathBuf>,
    },
    /// Execute the grep command to avoid the escape issue.
    #[structopt(name = "grep")]
    Grep {
        /// Specify the grep command to run, normally rg will be used.
        ///
        /// Incase of clap can not reconginize such option: --cmd "rg --vimgrep ... "fn ul"".
        ///                                                       |-----------------|
        ///                                                   this can be seen as an option by mistake.
        #[structopt(index = 1, short, long)]
        grep_cmd: String,

        /// Specify the query string for GREP_CMD.
        #[structopt(index = 2, short, long)]
        grep_query: String,

        /// Specify the working directory of CMD
        #[structopt(long = "cmd-dir", parse(from_os_str))]
        cmd_dir: Option<PathBuf>,
    },
}

#[derive(StructOpt, Debug)]
#[structopt(name = "maple")]
struct Maple {
    /// Print the top NUM of filtered items.
    ///
    /// The returned JSON has three fields:
    ///   - total: total number of initial filtered result set.
    ///   - lines: text lines used for displaying directly.
    ///   - indices: the indices of matched elements per line, used for the highlight purpose.
    #[structopt(short = "n", long = "number", name = "NUM")]
    number: Option<usize>,

    /// Prepend an icon for item of files and grep provider, valid only when --number is used.
    #[structopt(long = "enable-icon")]
    enable_icon: bool,

    #[structopt(subcommand)]
    command: Cmd,
}

#[derive(Debug)]
struct DummyError;

impl std::fmt::Display for DummyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DummyError is here!")
    }
}

impl std::error::Error for DummyError {
    fn description(&self) -> &str {
        "DummyError used for anyhow"
    }

    fn cause(&self) -> Option<&dyn std::error::Error> {
        None
    }
}

/// Remove the last element if it's empty string.
#[inline]
fn trim_trailing(lines: &mut Vec<String>) {
    if let Some(last_line) = lines.last() {
        if last_line.is_empty() || last_line == DEFAULT_ICONIZED {
            lines.remove(lines.len() - 1);
        }
    }
}

/// Combine json and println macro.
macro_rules! println_json {
  ( $( $field:expr ),+ ) => {
    {
      println!("{}", json!({ $(stringify!($field): $field,)* }))
    }
  }
}

fn tempfile(args: &[String]) -> Result<PathBuf> {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "{}_{}",
        args.join("_"),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs()
    ));
    Ok(dir)
}

fn cmd_output(cmd: &mut Command) -> Result<Output> {
    let cmd_output = cmd.output()?;

    // vim-clap does not handle the stderr stream, we just pass the error info via stdout.
    if !cmd_output.status.success() && !cmd_output.stderr.is_empty() {
        let error = format!("{}", String::from_utf8_lossy(&cmd_output.stderr));
        println_json!(error);
        std::process::exit(1);
    }

    Ok(cmd_output)
}

fn set_current_dir(cmd: &mut Command, cmd_dir: Option<PathBuf>) {
    if let Some(cmd_dir) = cmd_dir {
        // If cmd_dir is not a directory, use its parent as current dir.
        if cmd_dir.is_dir() {
            cmd.current_dir(cmd_dir);
        } else {
            let mut cmd_dir = cmd_dir;
            cmd_dir.pop();
            cmd.current_dir(cmd_dir);
        }
    }
}

fn prepare_grep_and_args(cmd_str: &str, cmd_dir: Option<PathBuf>) -> (Command, Vec<String>) {
    let args = cmd_str
        .split_whitespace()
        .map(Into::into)
        .collect::<Vec<String>>();

    let mut cmd = Command::new(args[0].clone());

    set_current_dir(&mut cmd, cmd_dir);

    (cmd, args)
}

// This can work with the piped command, e.g., git ls-files | uniq.
fn prepare_exec_cmd(cmd_str: &str, cmd_dir: Option<PathBuf>) -> Command {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut cmd = Command::new("cmd");
        cmd.args(&["/C", cmd_str]);
        cmd
    } else {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(cmd_str);
        cmd
    };

    set_current_dir(&mut cmd, cmd_dir);

    cmd
}

// Take the top number lines from stdout bytestream.
fn truncate_stdout(stdout: &[u8], number: usize) -> Vec<String> {
    // TODO: do not have to into String for whole stdout, find the nth index of newline.
    // &cmd_output.stdout[..nth_newline_index]
    let stdout_str = String::from_utf8_lossy(&stdout);
    let mut lines = stdout_str
        .split('\n')
        .take(number)
        .map(Into::into)
        .collect::<Vec<_>>();
    trim_trailing(&mut lines);
    lines
}

struct LightCommand<'a> {
    cmd: &'a mut Command,
}

fn try_cache(
    stdout: &[u8],
    total: usize,
    args: &[String],
    output: &Option<String>,
    output_threshold: usize,
) -> Result<(String, Option<PathBuf>)> {
    if output_threshold != 0 && total > output_threshold {
        let tempfile = if let Some(ref output) = output {
            output.into()
        } else {
            tempfile(args)?
        };
        File::create(tempfile.clone())?.write_all(stdout)?;
        Ok((
            // TODO: &cmd_output.stdout[..nth_newline_index]
            // lines used for displaying directly.
            String::from_utf8_lossy(stdout).to_string(),
            Some(tempfile),
        ))
    } else {
        Ok((String::from_utf8_lossy(stdout).to_string(), None))
    }
}

impl Maple {
    fn execute_impl(
        &self,
        cmd: &mut Command,
        args: &[String],
        output: &Option<String>,
        output_threshold: usize,
    ) -> Result<()> {
        let cmd_output = cmd_output(cmd)?;
        let cmd_stdout = &cmd_output.stdout;

        let total = bytecount::count(cmd_stdout, b'\n');

        if let Some(number) = self.number {
            let lines = truncate_stdout(cmd_stdout, number);
            println_json!(total, lines);
            return Ok(());
        }

        let (stdout_str, tempfile) = try_cache(cmd_stdout, total, args, output, output_threshold)?;

        let mut lines = if self.enable_icon {
            stdout_str.split('\n').map(prepend_icon).collect::<Vec<_>>()
        } else {
            stdout_str.split('\n').map(Into::into).collect::<Vec<_>>()
        };

        // The last element could be a empty string.
        trim_trailing(&mut lines);

        if let Some(tempfile) = tempfile {
            println_json!(total, lines, tempfile);
        } else {
            println_json!(total, lines);
        }

        Ok(())
    }

    fn apply_fuzzy_filter_and_rank(
        &self,
        query: &str,
        input: &Option<PathBuf>,
        algo: &Option<Algo>,
    ) -> Result<Vec<(String, f64, Vec<usize>)>> {
        let algo = algo.as_ref().unwrap_or(&Algo::Fzy);

        let scorer = |line: &str| match algo {
            Algo::Skim => {
                fuzzy_indices(line, &query).map(|(score, indices)| (score as f64, indices))
            }
            Algo::Fzy => match_and_score_with_positions(&query, line),
        };

        // Result<Option<T>> => T
        let mut ranked = if let Some(input) = input {
            std::fs::read_to_string(input)?
                .par_lines()
                .filter_map(|line| {
                    scorer(&line).map(|(score, indices)| (line.into(), score, indices))
                })
                .collect::<Vec<_>>()
        } else {
            io::stdin()
                .lock()
                .lines()
                .filter_map(|lines_iter| {
                    lines_iter.ok().and_then(|line| {
                        scorer(&line).map(|(score, indices)| (line, score, indices))
                    })
                })
                .collect::<Vec<_>>()
        };

        ranked.par_sort_unstable_by(|(_, v1, _), (_, v2, _)| v2.partial_cmp(&v1).unwrap());

        Ok(ranked)
    }

    fn run(&self) -> Result<()> {
        match &self.command {
            Cmd::Filter { query, input, algo } => {
                let ranked = self.apply_fuzzy_filter_and_rank(query, input, algo)?;

                if let Some(number) = self.number {
                    let total = ranked.len();
                    let payload = ranked.into_iter().take(number);
                    let mut lines = Vec::with_capacity(number);
                    let mut indices = Vec::with_capacity(number);
                    if self.enable_icon {
                        for (text, _, idxs) in payload {
                            lines.push(prepend_icon(&text));
                            indices.push(idxs);
                        }
                    } else {
                        for (text, _, idxs) in payload {
                            lines.push(text);
                            indices.push(idxs);
                        }
                    }
                    println_json!(total, lines, indices);
                } else {
                    for (text, _, indices) in ranked.iter() {
                        println_json!(text, indices);
                    }
                }
            }

            Cmd::Exec {
                cmd,
                output,
                cmd_dir,
                output_threshold,
            } => {
                let mut exec_cmd = prepare_exec_cmd(cmd, cmd_dir.clone());

                self.execute_impl(
                    &mut exec_cmd,
                    &cmd.split_whitespace().map(Into::into).collect::<Vec<_>>(),
                    output,
                    *output_threshold,
                )?;
            }

            Cmd::Grep {
                grep_cmd,
                grep_query,
                cmd_dir,
            } => {
                let (mut cmd, mut args) = prepare_grep_and_args(grep_cmd, cmd_dir.clone());

                // We split out the grep opts and query in case of the possible escape issue of clap.
                args.push(grep_query.clone());

                // currently vim-clap only supports rg.
                // Ref https://github.com/liuchengxu/vim-clap/pull/60
                if cfg!(windows) {
                    args.push(".".into());
                }

                cmd.args(&args[1..]);

                self.execute_impl(&mut cmd, &args, &None, 0usize)?;
            }
        }
        Ok(())
    }
}

pub fn main() -> Result<()> {
    let maple = Maple::from_args();

    maple.run()?;

    Ok(())
}

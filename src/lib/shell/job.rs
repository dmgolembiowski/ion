use super::Shell;
use crate::{
    builtins::BuiltinFunction,
    parser::{expand_string, pipelines::RedirectFrom},
    types,
};
use std::{fmt, fs::File, str};

#[derive(Clone)]
pub struct Job<'a> {
    pub args:        types::Args,
    pub redirection: RedirectFrom,
    pub builtin:     Option<BuiltinFunction<'a>>,
}

impl<'a> Job<'a> {
    /// Get the job command (its first arg)
    #[inline]
    pub fn command(&self) -> &types::Str { &self.args[0] }

    /// Takes the current job's arguments and expands them, one argument at a
    /// time, returning a new `Job` with the expanded arguments.
    pub(crate) fn expand(&mut self, shell: &Shell) {
        self.args = self.args.drain().flat_map(|arg| expand_arg(&arg, shell)).collect();
    }

    pub(crate) fn new(
        args: types::Args,
        redirection: RedirectFrom,
        builtin: Option<BuiltinFunction<'a>>,
    ) -> Self {
        Job { args, redirection, builtin }
    }
}

impl<'a> PartialEq for Job<'a> {
    fn eq(&self, other: &Job) -> bool {
        self.args == other.args && self.redirection == other.redirection
    }
}

impl<'a> fmt::Debug for Job<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Job {{ command: {}, args: {:?}, redirection: {:?} }}",
            self.args[0], self.args, self.redirection
        )
    }
}

/// Expands a given argument and returns it as an `Args`.
fn expand_arg(arg: &str, shell: &Shell) -> types::Args {
    let res = expand_string(&arg, shell);
    if res.is_empty() {
        args![""]
    } else {
        res
    }
}

/// This represents a job that has been processed and expanded to be run
/// as part of some pipeline
pub struct RefinedJob<'a> {
    pub stdin:  Option<File>,
    pub stdout: Option<File>,
    pub stderr: Option<File>,
    pub var:    JobVariant<'a>,
}

pub enum JobVariant<'a> {
    /// An external program that is executed by this shell
    External { args: types::Args },
    /// A procedure embedded into Ion
    Builtin { main: BuiltinFunction<'a>, args: types::Args },
    /// Functions can act as commands too!
    Function { args: types::Args },
    /// Represents redirection into stdin from more than one source
    Cat { sources: Vec<File> },
    Tee {
        /// 0 for stdout, 1 for stderr
        items: (Option<TeeItem>, Option<TeeItem>),
    },
}

#[derive(Debug)]
pub struct TeeItem {
    /// Where to read from for this tee. Generally only necessary if we need to tee both
    /// stdout and stderr.
    pub source: Option<File>,
    pub sinks: Vec<File>,
}

impl TeeItem {
    #[inline]
    pub(crate) fn new() -> Self { TeeItem { sinks: Vec::new(), source: None } }

    #[inline]
    pub(crate) fn add(&mut self, sink: File) { self.sinks.push(sink); }

    /// Writes out to all destinations of a Tee. Takes an extra `RedirectFrom` argument in
    /// order to
    /// handle piping. `RedirectFrom` paradoxically indicates where we are piping **to**. It
    /// should
    /// never be `RedirectFrom`::Both`
    pub(crate) fn write_to_all(&mut self, extra: Option<RedirectFrom>) -> ::std::io::Result<()> {
        use std::{
            io::{self, Read, Write},
            os::unix::io::*,
        };
        fn write_out<R>(source: &mut R, sinks: &mut [File]) -> io::Result<()>
        where
            R: Read,
        {
            let mut buf = [0; 4096];
            loop {
                // TODO: Figure out how to not block on this read
                let len = source.read(&mut buf)?;
                if len == 0 {
                    return Ok(());
                }
                for file in sinks.iter_mut() {
                    file.write_all(&buf[..len])?;
                }
            }
        }
        let stdout = io::stdout();
        let stderr = io::stderr();
        match extra {
            None => {}
            Some(RedirectFrom::Stdout) => unsafe {
                self.sinks.push(File::from_raw_fd(stdout.as_raw_fd()))
            },
            Some(RedirectFrom::Stderr) => unsafe {
                self.sinks.push(File::from_raw_fd(stderr.as_raw_fd()))
            },
            Some(RedirectFrom::Both) => {
                panic!("logic error! extra should never be RedirectFrom::Both")
            }
            Some(RedirectFrom::None) => panic!("logic error! No need to tee if no redirections"),
        };
        if let Some(ref mut file) = self.source {
            write_out(file, &mut self.sinks)
        } else {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            write_out(&mut stdin, &mut self.sinks)
        }
    }
}

impl<'a> RefinedJob<'a> {
    /// Returns a long description of this job: the commands and arguments
    pub(crate) fn long(&self) -> String {
        match self.var {
            JobVariant::External { ref args, .. }
            | JobVariant::Builtin { ref args, .. }
            | JobVariant::Function { ref args, .. } => args.join(" ").to_owned(),
            // TODO: Figure out real printing
            JobVariant::Cat { .. } | JobVariant::Tee { .. } => "".into(),
        }
    }

    pub(crate) fn stderr(&mut self, file: File) {
        if let JobVariant::Cat { .. } = self.var {
            return;
        }

        self.stderr = Some(file);
    }

    pub(crate) fn stdout(&mut self, file: File) { self.stdout = Some(file); }

    pub(crate) fn stdin(&mut self, file: File) { self.stdin = Some(file); }

    pub(crate) fn tee(tee_out: Option<TeeItem>, tee_err: Option<TeeItem>) -> Self {
        RefinedJob {
            stdin:  None,
            stdout: None,
            stderr: None,
            var:    JobVariant::Tee { items: (tee_out, tee_err) },
        }
    }

    pub(crate) fn cat(sources: Vec<File>) -> Self {
        RefinedJob { stdin: None, stdout: None, stderr: None, var: JobVariant::Cat { sources } }
    }

    pub(crate) fn function(args: types::Args) -> Self {
        RefinedJob {
            stdin:  None,
            stdout: None,
            stderr: None,
            var:    JobVariant::Function { args },
        }
    }

    pub(crate) fn builtin(main: BuiltinFunction<'a>, args: types::Args) -> Self {
        RefinedJob {
            stdin:  None,
            stdout: None,
            stderr: None,
            var:    JobVariant::Builtin { main, args },
        }
    }

    pub(crate) fn external(args: types::Args) -> Self {
        RefinedJob {
            stdin:  None,
            stdout: None,
            stderr: None,
            var:    JobVariant::External { args },
        }
    }
}

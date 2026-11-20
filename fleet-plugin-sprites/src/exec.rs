use crate::{
    CapturedOutput, WorkspaceCommand, WorkspaceExit, WorkspaceOutput, WorkspacesError,
    WorkspacesErrorKind,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Reads the command from standard input, runs it and reports buffered output.
///
/// Exports variables with a builtin so their values stay out of process arguments. Unsetting them
/// first keeps special shell variables from altering their values.
/// Prints keepalive lines while the command runs so idle connections stay open.
const SCRIPT: &str = r#"set -m
IFS= read -r -d '' t; IFS= read -r -d '' l; IFS= read -r -d '' c
IFS= read -r -d '' n; e=(); for ((i=0;i<n;i++)); do IFS= read -r -d '' v; e+=("$v"); done
IFS= read -r -d '' n; a=(); for ((i=0;i<n;i++)); do IFS= read -r -d '' v; a+=("$v"); done
cd -- "$c" || exit 1
d=$(mktemp -d) || exit 1
trap 'rm -rf -- "$d"' EXIT
x() { unset -v -- a e "${@%%=*}"; [ "$#" = 0 ] || export -- "$@"; }
(set -- "${a[@]}"; x "${e[@]}" 2>/dev/null || { echo 'could not set environment variables' >&2; exit 125; }; exec env -- "$@") <&0 >"$d/1" 2>"$d/2" & p=$!
(sleep "$t"; : >"$d/t"; kill -KILL -- -"$p") >/dev/null 2>&1 & w=$!
(while sleep 10; do echo .; done) & k=$!
wait "$p"; s=$?
kill -- -"$w" -"$k" 2>/dev/null; wait
if [ -e "$d/t" ]; then echo timeout; else echo "exit $s"; fi
for f in "$d/1" "$d/2"; do
z=$(stat -c %s -- "$f"); echo "$z"; h=$((z>l?l-l/2:z)); r=$((z>l?l/2:0))
head -c "$h" -- "$f" | base64 -w0; echo
tail -c +$((z-r+1)) -- "$f" | head -c "$r" | base64 -w0; echo
done
"#;

pub fn query() -> String {
    crate::sprites::query(&[
        ("cmd", "bash"),
        ("cmd", "-c"),
        ("cmd", SCRIPT),
        ("cmd", "bash"),
        ("stdin", "true"),
    ])
}

pub fn body(command: WorkspaceCommand, cwd: &str) -> Result<Vec<u8>, WorkspacesError> {
    if command.program.contains('=') {
        return Err(crate::error(
            WorkspacesErrorKind::Other,
            "program must not contain '='",
        ));
    }
    if let Some((name, _)) = command
        .env
        .iter()
        .find(|(name, _)| !crate::exec::valid_name(name))
    {
        return Err(crate::error(
            WorkspacesErrorKind::Other,
            format!("environment variable names must be shell variable names: {name}"),
        ));
    }
    let timeout = format!(
        "{}.{:03}",
        command.timeout_ms.div_euclid(1000),
        command.timeout_ms.rem_euclid(1000),
    );
    let mut fields = vec![
        timeout,
        command
            .output_limit
            .min(i64::MAX.unsigned_abs())
            .to_string(),
        cwd.to_owned(),
        command.env.len().to_string(),
    ];
    fields.extend(
        command
            .env
            .into_iter()
            .map(|(name, value)| format!("{name}={value}")),
    );
    fields.push(command.args.len().saturating_add(1).to_string());
    fields.push(command.program);
    fields.extend(command.args);
    if fields.iter().any(|field| field.contains('\0')) {
        return Err(crate::error(
            WorkspacesErrorKind::Other,
            "commands must not contain null bytes",
        ));
    }
    let mut body = Vec::new();
    for field in fields {
        body.extend(field.into_bytes());
        body.push(0);
    }
    body.extend(command.stdin);
    Ok(body)
}

/// Returns whether a name can be exported by the shell.
pub fn valid_name(name: &str) -> bool {
    name.starts_with(|first: char| first.is_ascii_alphabetic() || first == '_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub fn parse(response: &[u8]) -> Result<WorkspaceOutput, WorkspacesError> {
    let (stdout, stderr) = demultiplex(response);
    output(&stdout).ok_or_else(|| {
        crate::error(
            WorkspacesErrorKind::Other,
            format!(
                "Sprites command wrapper failed: {}",
                String::from_utf8_lossy(&stderr).trim(),
            ),
        )
    })
}

fn demultiplex(response: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut channel = None;
    for &byte in response {
        match byte {
            1 => channel = Some(&mut stdout),
            2 => channel = Some(&mut stderr),
            3 => break,
            byte => {
                if let Some(channel) = &mut channel {
                    channel.push(byte);
                }
            }
        }
    }
    (stdout, stderr)
}

fn output(stdout: &[u8]) -> Option<WorkspaceOutput> {
    let stdout = str::from_utf8(stdout).ok()?;
    let mut lines = stdout.split('\n').skip_while(|line| *line == ".");
    let exit = match lines.next()? {
        "timeout" => WorkspaceExit::TimedOut,
        line => WorkspaceExit::Code(line.strip_prefix("exit ")?.parse().ok()?),
    };
    let mut captured = || {
        let size: u64 = lines.next()?.parse().ok()?;
        let head = STANDARD.decode(lines.next()?).ok()?;
        let tail = STANDARD.decode(lines.next()?).ok()?;
        let retained = u64::try_from(head.len().saturating_add(tail.len())).ok()?;
        Some(CapturedOutput {
            head,
            omitted: size.saturating_sub(retained),
            tail,
        })
    };
    Some(WorkspaceOutput {
        exit,
        stdout: captured()?,
        stderr: captured()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> WorkspaceCommand {
        WorkspaceCommand {
            id: "1".into(),
            program: "printf".into(),
            args: vec!["%s\n".into(), "a b".into()],
            env: vec![("NAME".into(), "a=b".into())],
            cwd: None,
            stdin: b"input".to_vec(),
            timeout_ms: 1500,
            output_limit: 100,
        }
    }

    #[test]
    fn query_passes_script_as_an_argument() {
        let query = query();

        assert!(query.starts_with("cmd=bash&cmd=-c&cmd=set%20-m%0AIFS%3D%20read"));
        assert!(query.ends_with("&cmd=bash&stdin=true"));
    }

    #[test]
    fn body_encodes_fields_before_stdin() {
        let body = body(command(), "/home/sprite").unwrap();

        assert_eq!(
            body,
            b"1.500\x00100\x00/home/sprite\x001\x00NAME=a=b\x003\x00printf\x00%s\n\x00a b\x00input",
        );
    }

    #[test]
    fn body_limits_output_to_shell_integers() {
        let mut command = command();
        command.output_limit = u64::MAX;

        let body = body(command, "/home/sprite").unwrap();

        assert!(body.starts_with(b"1.500\x009223372036854775807\x00"));
    }

    #[test]
    fn body_rejects_ambiguous_commands() {
        let mut nul = command();
        nul.args.push("\0".into());
        let mut value = command();
        value.env.push(("A".into(), "\0".into()));
        let mut program = command();
        program.program = "A=b".into();
        let mut assignment = command();
        assignment.env.push(("A=b".into(), "c".into()));
        let mut name = command();
        name.env.push(("A-B".into(), "c".into()));

        for command in [nul, value, program, assignment, name] {
            let error = body(command, "/home/sprite").unwrap_err();

            assert!(matches!(error.kind, WorkspacesErrorKind::Other));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn script_passes_env_unchanged() {
        use std::{
            io::Write as _,
            process::{Command, Stdio},
        };

        let mut command = command();
        command.program = "printenv".into();
        command.env = [
            ("SECRET", "'$(a)\"\n=`b`"),
            ("LC_ALL", "secret"),
            ("OPTIND", "0007"),
            ("RANDOM", "1"),
            ("a", "b"),
            ("e", "f"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .into();
        command.args = command.env.iter().map(|(name, _)| name.clone()).collect();
        let mut bash = Command::new("bash")
            .args(["-c", SCRIPT, "bash"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = bash.stdin.take().unwrap();
        stdin.write_all(&body(command, "/").unwrap()).unwrap();
        drop(stdin);
        let response = [&[1], bash.wait_with_output().unwrap().stdout.as_slice()].concat();

        let output = parse(&response).unwrap();

        assert!(matches!(output.exit, WorkspaceExit::Code(0)));
        assert_eq!(
            String::from_utf8(output.stdout.head).unwrap(),
            "'$(a)\"\n=`b`\nsecret\n0007\n1\nb\nf\n",
        );
        assert_eq!(output.stderr.head, b"");
    }

    #[test]
    fn parse_reads_buffered_output() {
        let response = b"\x01.\n\x01.\nexit 2\n9\n\x01aGVs\nbG8=\n4\nb29wcw==\n\n\x02noise\x03\x00";

        let output = parse(response).unwrap();

        assert!(matches!(output.exit, WorkspaceExit::Code(2)));
        assert_eq!(output.stdout.head, b"hel");
        assert_eq!(output.stdout.omitted, 4);
        assert_eq!(output.stdout.tail, b"lo");
        assert_eq!(output.stderr.head, b"oops");
        assert_eq!(output.stderr.omitted, 0);
    }

    #[test]
    fn parse_reports_timeouts() {
        let response = b"\x01timeout\n0\n\n\n0\n\n\n\x03\x00";

        let output = parse(response).unwrap();

        assert!(matches!(output.exit, WorkspaceExit::TimedOut));
        assert_eq!(output.stdout.head, b"");
    }

    #[test]
    fn parse_reports_wrapper_errors() {
        let response = b"\x02bash: cd: /missing: No such file or directory\n\x03\x01";

        let error = parse(response).unwrap_err();

        assert!(matches!(error.kind, WorkspacesErrorKind::Other));
        assert_eq!(
            error.message,
            "Sprites command wrapper failed: bash: cd: /missing: No such file or directory",
        );
    }
}

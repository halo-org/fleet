use crate::{
    WorkspaceCommand, WorkspaceExit, WorkspaceOutput, WorkspacesError, WorkspacesErrorKind,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Reads the command from standard input, runs it and reports buffered output.
///
/// Prints keepalive lines while the command runs so idle connections stay open.
const SCRIPT: &str = r#"set -m
IFS= read -r -d '' t; IFS= read -r -d '' l; IFS= read -r -d '' c
IFS= read -r -d '' n; e=(); for ((i=0;i<n;i++)); do IFS= read -r -d '' v; e+=("$v"); done
IFS= read -r -d '' n; a=(); for ((i=0;i<n;i++)); do IFS= read -r -d '' v; a+=("$v"); done
cd -- "$c" || exit 1
d=$(mktemp -d) || exit 1
trap 'rm -rf -- "$d"' EXIT
env -- "${e[@]}" "${a[@]}" <&0 >"$d/1" 2>"$d/2" & p=$!
(sleep "$t"; : >"$d/t"; kill -KILL -- -"$p") >/dev/null 2>&1 & w=$!
(while sleep 10; do echo .; done) & k=$!
wait "$p"; s=$?
kill -- -"$w" -"$k" 2>/dev/null; wait
if [ -e "$d/t" ]; then echo timeout; else echo "exit $s"; fi
stat -c %s -- "$d/1" "$d/2"
head -c "$l" -- "$d/1" | base64 -w0; echo
head -c "$l" -- "$d/2" | base64 -w0; echo
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
    if command.program.contains('=')
        || command
            .env
            .iter()
            .any(|(name, _)| name.is_empty() || name.contains('='))
    {
        return Err(crate::error(
            WorkspacesErrorKind::Other,
            "program and environment variable names must not contain '='",
        ));
    }
    let timeout = format!(
        "{}.{:03}",
        command.timeout_ms.div_euclid(1000),
        command.timeout_ms.rem_euclid(1000),
    );
    let mut fields = vec![
        timeout,
        command.output_limit.to_string(),
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

pub fn parse(response: &[u8], limit: u64) -> Result<WorkspaceOutput, WorkspacesError> {
    let (stdout, stderr) = demultiplex(response);
    output(&stdout, limit).ok_or_else(|| {
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

fn output(stdout: &[u8], limit: u64) -> Option<WorkspaceOutput> {
    let stdout = str::from_utf8(stdout).ok()?;
    let mut lines = stdout.split('\n').skip_while(|line| *line == ".");
    let exit = match lines.next()? {
        "timeout" => WorkspaceExit::TimedOut,
        line => WorkspaceExit::Code(line.strip_prefix("exit ")?.parse().ok()?),
    };
    let stdout_size: u64 = lines.next()?.parse().ok()?;
    let stderr_size: u64 = lines.next()?.parse().ok()?;
    let stdout = STANDARD.decode(lines.next()?).ok()?;
    let stderr = STANDARD.decode(lines.next()?).ok()?;
    Some(WorkspaceOutput {
        exit,
        stdout,
        stderr,
        stdout_truncated: stdout_size > limit,
        stderr_truncated: stderr_size > limit,
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
    fn body_rejects_ambiguous_commands() {
        let mut nul = command();
        nul.args.push("\0".into());
        let mut program = command();
        program.program = "A=b".into();
        let mut name = command();
        name.env.push(("A=b".into(), "c".into()));

        for command in [nul, program, name] {
            let error = body(command, "/home/sprite").unwrap_err();

            assert!(matches!(error.kind, WorkspacesErrorKind::Other));
        }
    }

    #[test]
    fn parse_reads_buffered_output() {
        let response = b"\x01.\n\x01.\nexit 2\n6\n\x014\naGVsbG8=\nb29wcw==\n\x02noise\x03\x00";

        let output = parse(response, 5).unwrap();

        assert!(matches!(output.exit, WorkspaceExit::Code(2)));
        assert_eq!(output.stdout, b"hello");
        assert_eq!(output.stderr, b"oops");
        assert!(output.stdout_truncated);
        assert!(!output.stderr_truncated);
    }

    #[test]
    fn parse_reports_timeouts() {
        let response = b"\x01timeout\n0\n0\n\n\n\x03\x00";

        let output = parse(response, 5).unwrap();

        assert!(matches!(output.exit, WorkspaceExit::TimedOut));
        assert_eq!(output.stdout, b"");
    }

    #[test]
    fn parse_reports_wrapper_errors() {
        let response = b"\x02bash: cd: /missing: No such file or directory\n\x03\x01";

        let error = parse(response, 5).unwrap_err();

        assert!(matches!(error.kind, WorkspacesErrorKind::Other));
        assert_eq!(
            error.message,
            "Sprites command wrapper failed: bash: cd: /missing: No such file or directory",
        );
    }
}

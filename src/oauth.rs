//! Browser sign-in, so an account that was never signed in on this machine can still be
//! read. The mechanics live here: a PKCE pair, the URL to open, a loopback listener for
//! the vendors that call back on their own, and the hand-off to the desktop browser.
//!
//! Which URLs, scopes and client id a provider uses, and what its token response turns
//! into, belong to the provider module: this file never talks to a vendor itself.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a sign-in waits for the browser before giving up and saying so.
const WINDOW: Duration = Duration::from_secs(300);

/// How often the listener looks up from the socket to see whether the user gave up.
const TICK: Duration = Duration::from_millis(100);

/// One provider's authorization-code flow, as its own CLI runs it.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub authorize: &'static str,
    pub token: &'static str,
    pub client_id: &'static str,
    pub scopes: &'static str,
    /// Where the vendor sends the browser back. A loopback URL is served by this
    /// process; a hosted one shows the code for the user to carry back by hand.
    pub redirect: &'static str,
    /// Query parameters the vendor's own CLI sends, which some flows are gated on.
    pub extra: &'static [(&'static str, &'static str)],
    /// The port to serve the redirect on, when the redirect is a loopback URL.
    pub callback_port: Option<u16>,
}

/// A sign-in in progress: what to open, and how its answer comes back.
pub struct SignIn {
    spec: Spec,
    /// Kept until the token exchange, and sent nowhere else. This is what proves the
    /// code that comes back belongs to this process.
    verifier: String,
    state: String,
    pub url: String,
    cancel: Arc<AtomicBool>,
}

impl SignIn {
    pub fn start(spec: Spec) -> Result<Self, String> {
        let verifier = base64url(&random_bytes(32)?);
        let state = base64url(&random_bytes(16)?);
        let challenge = base64url(&sha256(verifier.as_bytes()));
        let url = authorize_url(&spec, &challenge, &state);
        Ok(Self {
            spec,
            verifier,
            state,
            url,
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The verifier the token exchange has to prove itself with.
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// True when the vendor calls back here on its own, so the user has nothing to carry
    /// back from the browser.
    pub fn calls_back(&self) -> bool {
        self.spec.callback_port.is_some()
    }

    /// Serve the redirect once and hand back the code the vendor sent. Called before the
    /// browser opens, so a port that is taken fails the sign-in instead of the browser.
    pub fn listen(&self) -> Result<Receiver<Result<String, String>>, String> {
        let port = self
            .spec
            .callback_port
            .ok_or("this provider does not call back on its own")?;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| format!("port {port} is not free for the sign-in callback: {e}"))?;
        let (tx, rx) = mpsc::channel();
        let state = self.state.clone();
        let cancel = Arc::clone(&self.cancel);
        std::thread::spawn(move || {
            let _ = tx.send(serve(listener, &state, &cancel));
        });
        Ok(rx)
    }

    /// The user walked away, or asked to stop waiting.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Wait for one callback on `listener`, then answer the browser. The code travels back
/// over the channel the sign-in holds.
fn serve(listener: TcpListener, state: &str, cancel: &AtomicBool) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("could not watch the callback port: {e}"))?;
    let deadline = Instant::now() + WINDOW;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        if Instant::now() >= deadline {
            return Err("no answer from the browser in five minutes".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let target = request_target(&mut stream);
                match code_from_query(&target, state) {
                    Ok(code) => {
                        respond(&mut stream, true);
                        return Ok(code);
                    }
                    Err(why) => {
                        // A request that is not the callback (a favicon, a probe) is not
                        // worth ending the wait over.
                        if target.contains("code=") || target.contains("error=") {
                            respond(&mut stream, false);
                            return Err(why);
                        }
                        respond(&mut stream, false);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(TICK),
            Err(e) => return Err(format!("the callback port stopped answering: {e}")),
        }
    }
}

/// The request line's target, e.g. `/auth/callback?code=abc&state=xyz`.
fn request_target(stream: &mut TcpStream) -> String {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return String::new();
    }
    line.split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

fn respond(stream: &mut TcpStream, ok: bool) {
    let note = if ok {
        "usagebar is signed in. This tab can be closed."
    } else {
        "usagebar could not use that sign-in. Back to the terminal for the reason."
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>usagebar</title>\
         <body style=\"font:14px system-ui;padding:3rem\">{note}</body>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {} \r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        if ok { "200 OK" } else { "400 Bad Request" },
        body.len(),
        body
    );
}

/// The code from a callback URL's query string. The state has to be the one this sign-in
/// generated, or a stale tab could hand us a code that is not ours.
fn code_from_query(target: &str, expected_state: &str) -> Result<String, String> {
    let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
    let (mut code, mut state, mut error, mut description) = (None, None, None, None);
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        match key {
            "code" => code = Some(value),
            "state" => state = Some(value),
            "error" => error = Some(value),
            "error_description" => description = Some(value),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Err(match description {
            Some(description) => format!("{error}: {description}"),
            None => error,
        });
    }
    match state.as_deref() {
        Some(seen) if seen == expected_state => {}
        Some(_) => return Err("the sign-in answer was for another request".into()),
        None => return Err("the sign-in answer carried no state".into()),
    }
    code.filter(|code| !code.is_empty())
        .ok_or_else(|| "the sign-in answer carried no code".into())
}

fn authorize_url(spec: &Spec, challenge: &str, state: &str) -> String {
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", spec.client_id),
        ("redirect_uri", spec.redirect),
        ("scope", spec.scopes),
        ("state", state),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ];
    params.extend_from_slice(spec.extra);
    let query: Vec<String> = params
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                crate::providers::urlencode(key),
                crate::providers::urlencode(value)
            )
        })
        .collect();
    format!("{}?{}", spec.authorize, query.join("&"))
}

/// Hand the URL to the desktop's browser. A machine with no browser is not a failure:
/// the URL is on screen, and the sign-in waits either way.
pub fn open_browser(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

// ------------------------------------------------------------------ pkce

fn random_bytes(len: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf).map_err(|e| format!("no system randomness: {e}"))?;
    Ok(buf)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

/// Base64url without padding: the encoding PKCE is defined in, and the one a JWT's
/// payload uses. The decoder for the latter lives with the token reading in `providers`.
pub(crate) fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let packed = u32::from(block[0]) << 16 | u32::from(block[1]) << 8 | u32::from(block[2]);
        for index in 0..chunk.len() + 1 {
            let sextet = (packed >> (18 - index * 6)) & 0x3F;
            out.push(ALPHABET[sextet as usize] as char);
        }
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(callback_port: Option<u16>) -> Spec {
        Spec {
            authorize: "https://auth.example.com/authorize",
            token: "https://auth.example.com/token",
            client_id: "client-1",
            scopes: "openid profile email",
            redirect: "http://localhost:1455/auth/callback",
            extra: &[("simplified_flow", "true")],
            callback_port,
        }
    }

    /// The RFC 7636 example, so the challenge is proven against the spec rather than
    /// against our own decoder.
    #[test]
    fn pkce_matches_the_spec_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64url(&sha256(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        // Encoding stops at whole sextets: no padding, and nothing else either.
        assert_eq!(base64url(b"a"), "YQ");
        assert_eq!(base64url(b"ab"), "YWI");
        assert_eq!(base64url(b""), "");
    }

    #[test]
    fn the_url_carries_what_the_flow_needs() {
        let url = authorize_url(&spec(None), "challenge-1", "state-1");
        assert!(url.starts_with("https://auth.example.com/authorize?"));
        for expected in [
            "client_id=client-1",
            "response_type=code",
            "code_challenge=challenge-1",
            "code_challenge_method=S256",
            "state=state-1",
            // The scopes' spaces are encoded, not left to break the query.
            "scope=openid%20profile%20email",
            "redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
            "simplified_flow=true",
        ] {
            assert!(url.contains(expected), "{expected} is missing from {url}");
        }
    }

    #[test]
    fn a_sign_in_generates_a_fresh_verifier_and_state() {
        let first = SignIn::start(spec(None)).unwrap();
        let second = SignIn::start(spec(None)).unwrap();
        assert_ne!(first.verifier(), second.verifier());
        assert_ne!(first.url, second.url);
        assert!(!first.calls_back());
        assert!(SignIn::start(spec(Some(1455))).unwrap().calls_back());
    }

    #[test]
    fn the_callback_code_is_read_and_the_state_checked() {
        let code = code_from_query("/auth/callback?code=abc-123&state=s1", "s1").unwrap();
        assert_eq!(code, "abc-123");
        // Escaped code, and parameters in any order.
        assert_eq!(
            code_from_query("/auth/callback?state=s1&code=a%2Fb%2Bc", "s1").unwrap(),
            "a/b+c"
        );
        // A code from a sign-in this process did not start is refused.
        assert!(code_from_query("/auth/callback?code=abc&state=other", "s1").is_err());
        assert!(code_from_query("/auth/callback?code=abc", "s1").is_err());
        // The vendor's own refusal arrives with its words.
        assert_eq!(
            code_from_query(
                "/auth/callback?error=access_denied&error_description=nope",
                "s1"
            )
            .unwrap_err(),
            "access_denied: nope"
        );
        // A request for something else is not a code.
        assert!(code_from_query("/favicon.ico", "s1").is_err());
    }

    /// The listener answers the browser and hands the code back to the waiter.
    #[test]
    fn the_loopback_listener_returns_the_code_once() {
        // A free port, so the test never fights a real sign-in for the vendor's own.
        let port = {
            let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            probe.local_addr().unwrap().port()
        };
        let signin = SignIn::start(spec(Some(port))).unwrap();
        let state = signin.state.clone();
        let rx = signin.listen().unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client
            .write_all(
                format!("GET /auth/callback?code=code-9&state={state} HTTP/1.1\r\n\r\n").as_bytes(),
            )
            .unwrap();
        let mut reply = String::new();
        let _ = BufReader::new(&client).read_line(&mut reply);
        assert!(
            reply.contains("200"),
            "the browser was not answered: {reply}"
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            "code-9"
        );
    }
}

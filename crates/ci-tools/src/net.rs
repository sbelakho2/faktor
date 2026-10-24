use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::time::Duration;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_TIMEOUT_MS: u64 = 2_000;

fn usage() {
    eprintln!(
        "usage: faktor-ci-tools tcp-connect [--host HOST] --port PORT [--timeout-ms MS]\n       faktor-ci-tools tcp-connect selftest"
    );
}

fn connect(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let address = format!("{host}:{port}");
    match TcpStream::connect_timeout(
        &address
            .to_socket_addrs()
            .map_err(|error| format!("{address}: {error}"))?
            .next()
            .ok_or_else(|| format!("{address}: no resolved address"))?,
        timeout,
    ) {
        Ok(_) => Ok(()),
        Err(error) => Err(format!("{address}: {error}")),
    }
}

fn selftest() -> i32 {
    let mut failures = 0;
    let mut check = |name: &str, ok: bool| {
        if ok {
            println!("selftest ok: {name}");
        } else {
            eprintln!("selftest FAIL: {name}");
            failures += 1;
        }
    };

    let listener = match TcpListener::bind((DEFAULT_HOST, 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("tcp-connect selftest: bind loopback listener: {error}");
            return 1;
        }
    };
    let port = listener
        .local_addr()
        .expect("bound listener address")
        .port();
    let (ready, wait) = mpsc::channel();
    let acceptor = std::thread::spawn(move || {
        let _ = ready.send(());
        let _ = listener.accept();
    });
    let _ = wait.recv_timeout(Duration::from_secs(2));
    check(
        "a live loopback listener accepts the probe",
        connect(DEFAULT_HOST, port, Duration::from_secs(2)).is_ok(),
    );
    let _ = acceptor.join();

    let closed = match TcpListener::bind((DEFAULT_HOST, 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("tcp-connect selftest: bind closed listener: {error}");
            return 1;
        }
    };
    let closed_port = closed.local_addr().expect("bound listener address").port();
    drop(closed);
    check(
        "a closed loopback port is refused",
        connect(DEFAULT_HOST, closed_port, Duration::from_millis(500)).is_err(),
    );

    if failures > 0 {
        eprintln!("tcp-connect selftest: FAIL ({failures})");
        return 1;
    }
    println!("tcp-connect selftest: PASS");
    0
}

pub fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("selftest") {
        return selftest();
    }
    let mut host = DEFAULT_HOST.to_string();
    let mut port = None;
    let mut timeout = Duration::from_millis(DEFAULT_TIMEOUT_MS);
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--host" if index + 1 < args.len() => {
                host = args[index + 1].clone();
                index += 2;
            }
            "--port" if index + 1 < args.len() => {
                match args[index + 1].parse::<u16>() {
                    Ok(value) => port = Some(value),
                    Err(error) => {
                        eprintln!("tcp-connect: invalid port: {error}");
                        return 2;
                    }
                }
                index += 2;
            }
            "--timeout-ms" if index + 1 < args.len() => {
                match args[index + 1].parse::<u64>() {
                    Ok(value) => timeout = Duration::from_millis(value),
                    Err(error) => {
                        eprintln!("tcp-connect: invalid timeout: {error}");
                        return 2;
                    }
                }
                index += 2;
            }
            other => {
                eprintln!("tcp-connect: unknown argument '{other}'");
                usage();
                return 2;
            }
        }
    }
    let Some(port) = port else {
        usage();
        return 2;
    };
    match connect(&host, port, timeout) {
        Ok(()) => {
            println!("tcp-connect: connected to {host}:{port}");
            0
        }
        Err(error) => {
            println!("tcp-connect: {error}");
            1
        }
    }
}

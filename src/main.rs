extern crate core;

use std::env;
use std::error::Error;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use qrcode::render::unicode;
use qrcode::QrCode;
use ricq::client::Token;
use ricq::device::Device;
use ricq::ext::common::after_login;
use ricq::handler::QEvent;
use ricq::{version, Client, LoginResponse, QRCodeConfirmed, QRCodeImageFetch, QRCodeState};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpSocket;
use tokio::task::yield_now;
use tokio::{fs, io, runtime};
use tracing::{debug, error, info, warn, Level};

static HELP_INFO: &str = "\
Atri Login Helper
help -> Show this info
login <uin> <password> -> Login with password
qrlogin [uin] -> Login with qrcode

exit | quit -> Close this program
-------------------------------------------------
After login, the device info will automatically generate and write
to 'device.json', and the login token will write to 'token.json'
";

static WELCOME_INFO: &str = "\
--------------------------
Welcome to Atri Login Helper
--------------------------
This program can help you to login atri_bot
and generate the device info.

Author: LaoLittle (https://github.com/LaoLittle)
";

macro_rules! unwrap_option_or_help {
    ($($x:tt)+) => {
        match ($($x)+) {
            Some(s) => s,
            None => {
                println!("{}", HELP_INFO);
                continue;
            },
        }
    };
}

macro_rules! unwrap_result_or_err {
    ($($x:tt)+) => {
        match ($($x)+) {
            Ok(s) => s,
            Err(e) => {
                error!("{:?}", e);
                continue;
            },
        }
    };
}

type MainResult = Result<(), Box<dyn Error>>;

fn main() -> MainResult {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(true)
        .with_max_level(Level::DEBUG)
        .init();

    let rt = runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;

    println!("{}", WELCOME_INFO);

    rt.block_on(main0())
}

async fn main0() -> MainResult {
    let mut stdout = tokio::io::stdout();
    let stdin = tokio::io::stdin();
    let mut stdin = BufReader::new(stdin);

    'main: loop {
        stdout.write_all(b">> ").await?;
        stdout.flush().await?;

        let mut buf = String::new();
        stdin.read_line(&mut buf).await?;

        let s = buf.trim_end();

        let spl: Vec<&str> = s.split(' ').collect();
        let first = *spl.first().expect("Cannot be None");

        let mut p = PathBuf::new();
        match first {
            "" => println!(),
            "help" => println!("{}", HELP_INFO),
            "exit" | "quit" => break,
            "login" => {
                let account = unwrap_option_or_help!(spl.get(1));
                p.push(account);
                if !p.is_dir() {
                    fs::create_dir(&p).await?;
                }

                let account = unwrap_result_or_err!(i64::from_str(account));

                let password = unwrap_option_or_help!(spl.get(2));

                let device = device_or_default(&p).await;
                let client = get_client(device).await?;

                let resp = client.password_login(account, password).await?;

                println!("{:?}", resp);
            }
            "qrlogin" => {
                let device = match spl.get(1) {
                    Some(account) => {
                        unwrap_result_or_err!(i64::from_str(account));
                        p.push(account);
                        if !p.is_dir() {
                            fs::create_dir(&p).await?;
                        }
                        device_or_default(&p).await
                    }
                    None => Device::random(),
                };

                let client = get_client(device).await?;

                let mut state = client.fetch_qrcode().await?;

                let f_name = "qr.png";
                let mut img_file = fs::File::create(f_name).await?;
                let mut signature = None::<Bytes>;
                loop {
                    match state {
                        QRCodeState::ImageFetch(QRCodeImageFetch {
                            ref image_data,
                            ref sig,
                        }) => {
                            img_file.write_all(image_data).await?;
                            signature = Some(sig.clone());
                            let mut qr_file = env::current_dir().unwrap_or_default();
                            qr_file.push(f_name);

                            info!("已获取二维码，位于 {}", qr_file.to_str().unwrap_or(f_name));

                            if let Ok(s) = get_qr(image_data) {
                                println!("{}", s);
                            }
                        }
                        QRCodeState::WaitingForScan => {
                            info!("等待扫码");
                        }
                        QRCodeState::WaitingForConfirm => {
                            info!("已扫码，等待确认");
                        }
                        QRCodeState::Confirmed(QRCodeConfirmed {
                            ref tmp_pwd,
                            ref tmp_no_pic_sig,
                            ref tgt_qr,
                            ..
                        }) => {
                            let mut login_resp =
                                client.qrcode_login(tmp_pwd, tmp_no_pic_sig, tgt_qr).await?;

                            match login_resp {
                                LoginResponse::DeviceLockLogin(..) => {
                                    login_resp = client.device_lock_login().await?;
                                }
                                LoginResponse::Success(..) => {}
                                _ => {
                                    panic!("Unknown status: {:?}", login_resp);
                                }
                            }

                            debug!("{:?}", login_resp);
                            info!("登陆成功");
                            break;
                        }
                        QRCodeState::Canceled => {
                            error!("已取消扫码");
                            continue 'main;
                        }
                        QRCodeState::Timeout => {
                            error!("超时，重新生成二维码");

                            state = client.fetch_qrcode().await?;
                            if let QRCodeState::ImageFetch(ref fe) = state {
                                img_file.write_all(&fe.image_data).await?;
                                signature = Some(fe.sig.clone());

                                if let Ok(s) = get_qr(&fe.image_data) {
                                    println!("{}", s);
                                }
                            }
                        }
                    }

                    tokio::time::sleep(Duration::from_secs(5)).await;

                    if let Some(ref sig) = signature {
                        state = client.query_qrcode_result(sig).await?;
                    }
                }

                after_login(&client).await;

                let account = client.uin().await;
                p.push(account.to_string());
                if !p.is_dir() {
                    fs::create_dir(&p).await?;
                }
                {
                    p.push("device.json");
                    let mut f = fs::File::create(&p).await?;

                    let device = client.device().await;
                    let s = serde_json::to_string_pretty(&device)?;
                    f.write_all(s.as_bytes()).await?;
                    p.pop();
                }

                let token = client.gen_token().await;
                write_token_file(&token, &p).await?;
            }
            _ => println!(
                "Unknown command '{}', use 'help' to show the help info",
                first
            ),
        }
    }

    Ok(())
}

async fn device_or_default<P: AsRef<Path>>(dir: P) -> Device {
    let mut buf = PathBuf::new();
    buf.push(dir);
    buf.push("device.json");

    if !buf.is_file() {
        let d = Device::random();
        let s = serde_json::to_string_pretty(&d).expect("Serialization fault");
        let mut f = fs::File::create(&buf).await.expect("Cannot create file");
        f.write_all(s.as_bytes()).await.ok();

        return d;
    }

    let mut f = fs::File::open(&buf).await.expect("Cannot open file");
    let mut s = String::new();
    f.read_to_string(&mut s).await.expect("Cannot read file");

    serde_json::from_str(&s).unwrap_or_else(|_| Device::random())
}

async fn get_client(device: Device) -> io::Result<Arc<Client>> {
    struct Nop;

    impl ricq::handler::Handler for Nop {
        fn handle<'a: 'b, 'b>(
            &'a self,
            _event: QEvent,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'b>> {
            Box::pin(async {})
        }
    }

    let client = Client::new(device, version::MACOS, Nop);
    let client = Arc::new(client);

    //let addr = SocketAddr::new(Ipv4Addr::new(113, 96, 18, 253).into(), 80);

    let mut addrs = client.get_address_list().await;
    let total = addrs.len();
    let mut now = 0;

    let stream = loop {
        let socket = TcpSocket::new_v4()?;
        match socket
            .connect(addrs.pop().ok_or(io::ErrorKind::AddrNotAvailable)?)
            .await
        {
            Ok(s) => break s,
            Err(e) => {
                now += 1;
                warn!("连接失败: {}, 尝试重连({}/{})", e, now, total);
            }
        }
    };

    let client0 = client.clone();
    tokio::spawn(async move {
        client0.start(stream).await;
    });
    yield_now().await;

    Ok(client)
}

async fn write_token_file<P: AsRef<Path>>(token: &Token, dir: P) -> io::Result<()> {
    let mut buf = PathBuf::new();
    buf.push(dir);
    buf.push("token.json");

    let mut f = fs::File::create(&buf).await?;
    let s = serde_json::to_string_pretty(token).expect("Serialization fault");

    f.write_all(s.as_bytes()).await?;

    Ok(())
}

fn get_qr<B: AsRef<[u8]>>(b: B) -> Result<String, Box<dyn Error>> {
    let img = image::load_from_memory(b.as_ref())?.to_luma8();

    let mut img = rqrr::PreparedImage::prepare(img);
    let grids = img.detect_grids();

    let s = grids[0].decode()?.1;

    let qr = QrCode::new(s.as_bytes())?;
    Ok(qr
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build())
}

mod compat;

extern crate core;

use std::{env};
use std::error::Error;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use qrcode::QrCode;
use qrcode::render::unicode;
use ricq::{Client, LoginResponse, QRCodeConfirmed, QRCodeImageFetch, QRCodeState, version};
use ricq::client::Token;
use ricq::device::Device;
use ricq::ext::common::after_login;
use ricq::handler::DefaultHandler;
use tokio::{fs, io, runtime};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpSocket;
use tokio::task::yield_now;
use tracing::{debug, error, Event, Id, info, Level, Metadata, Subscriber};
use tracing::span::{Attributes, Record};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use crate::compat::mirai::MiraiDeviceInfo;

static HELP_INFO: &str = "\
RQ Login Helper
help -> Show this info
login <qq> <password> -> Login with password
qrlogin [qq] -> Login with qrcode

exit | quit -> Close this program
-------------------------------------------------
After login, the device info will automatically generate and write
to 'device.json', and the login token will write to 'token.json'
";

static WELCOME_INFO: &str = "\
--------------------------
Welcome to RQ Login Helper
--------------------------
This program can help you to login qq
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
    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::filter_fn(|m| {
            match m.level() {
                &Level::TRACE => false,
                _ => true
            }
        }))
        .with(tracing_subscriber::fmt::layer().with_target(true))
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

    'main:
    loop {
        //print!(">>");
        stdout.write_all(b">>").await?;
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
                if !p.is_dir() { fs::create_dir(&p).await?; }

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
                        if !p.is_dir() { fs::create_dir(&p).await?; }
                        device_or_default(&p).await
                    }
                    None => {
                        Device::random()
                    }
                };

                let client = get_client(device).await?;

                let mut state = client.fetch_qrcode().await?;

                let f_name = "qr.png";
                let mut img_file = fs::File::create(f_name).await?;
                let mut signature = None::<Bytes>;
                loop {
                    match state {
                        QRCodeState::ImageFetch(
                            QRCodeImageFetch {
                                ref image_data,
                                ref sig
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
                        QRCodeState::Confirmed(
                            QRCodeConfirmed {
                                ref tmp_pwd,
                                ref tmp_no_pic_sig,
                                ref tgt_qr,
                                ..
                            }) => {
                            let mut login_resp = client.qrcode_login(
                                tmp_pwd,
                                tmp_no_pic_sig,
                                tgt_qr,
                            ).await?;

                            if let LoginResponse::DeviceLockLogin(..) = login_resp {
                                login_resp = client.device_lock_login().await?;
                            } else {
                                panic!("Not device lock login: {:?}", login_resp);
                            }

                            info!("登陆成功");
                            debug!("{:?}", login_resp);

                            break;
                        }
                        QRCodeState::Canceled => {
                            error!("已取消扫码");
                            continue 'main;
                        }
                        QRCodeState::Timeout => {
                            state = client.fetch_qrcode().await?;
                            if let QRCodeState::ImageFetch(ref fe) = state {
                                img_file.write_all(&fe.image_data).await?;
                                signature = Some(fe.sig.clone());

                                if let Ok(s) = get_qr(&fe.image_data) {
                                    println!("{}", s);
                                }
                            }

                            error!("超时，重新生成二维码");
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
                if !p.is_dir() { fs::create_dir(&p).await?; }
                {
                    p.push("device.json");
                    let mut f = fs::File::create(&p).await?;

                    let device = client.device().await;
                    let s = serde_json::to_string_pretty(&device)?;
                    f.write_all(s.as_bytes()).await?;
                    p.pop();

                    p.push("device.mirai.json");
                    let mut f = fs::File::create(&p).await?;

                    let mirai: MiraiDeviceInfo = device.into();
                    let s = serde_json::to_string_pretty(&mirai)?;
                    f.write_all(s.as_bytes()).await?;
                    p.pop();
                }

                let token = client.gen_token().await;
                write_token_file(&token, &p).await?;
            }
            _ => println!("Unknown command '{}', use 'help' to show the help info", first)
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
    let client = Client::new(
        device,
        version::ANDROID_WATCH,
        DefaultHandler,
    );
    let client = Arc::new(client);

    //let addr = SocketAddr::new(Ipv4Addr::new(113, 96, 18, 253).into(), 80);
    let socket = TcpSocket::new_v4()?;
    let stream = socket.connect(client.get_address()).await?;

    let client0 = client.clone();
    tokio::spawn(async move { client0.start(stream).await; });
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
    Ok(
        qr.render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build()
    )
}

#[cfg(test)]
mod test {

    use qrcode::QrCode;
    use qrcode::render::unicode;

    use ricq::device::Device;
    use crate::compat::mirai::{MiraiDeviceInfo};

    #[test]
    fn qr() {
        let img = image::open("qr.png").unwrap().to_luma8();

        let mut img = rqrr::PreparedImage::prepare(img);
        let grids = img.detect_grids();

        let s = grids[0].decode().unwrap().1;

        let qr = QrCode::new(s.as_bytes()).unwrap();
        let s = qr.render::<unicode::Dense1x2>()
            .dark_color(unicode::Dense1x2::Light)
            .light_color(unicode::Dense1x2::Dark)
            .build();
        println!("{}", s);
    }

    #[test]
    fn byte_8() {
        let a = [
            12u8,
            64,
            241,
            114,
            202,
            68,
            189,
            13,
            133,
            122,
            67,
            241,
            146,
            140,
            247,
            134
        ];

        let mut str = String::new();

        for byte in a {
            let s = format!("{:02x}", byte);
            str.push_str(&s);
        }
        println!("{}", str.len());
    }

    #[test]
    fn d_to_mirai() {
        let d = Device::random();
        let mirai: MiraiDeviceInfo = d.into();



        println!("{:#?}", mirai);
    }

    #[test]
    fn mirai_d() {
        let mirai_device = r#"
        {
    "deviceInfoVersion": 2,
    "data": {
        "display": "OPR1.170623.027",
        "product": "Huawei Mediapad M3",
        "device": "hwbeethoven",
        "board": "hi3650",
        "brand": "Huawei",
        "model": "hwbeethoven",
        "bootloader": "unknown",
        "fingerprint": "Huawei/BTV/hi3650:6.0/MRA58K/huawei12151809:user/release-keys",
        "bootId": "071BD136-30B7-4778-7037-B8AC5A714E27",
        "procVersion": "Linux version 3.0.31-7EIDAfp4 (android-build@xxx.xxx.xxx.xxx.com)",
        "baseBand": "",
        "version": {
            "incremental": "eng.huawei.20161215.180805",
            "release": "6.0",
            "codename": "REL",
            "sdk": 23
        },
        "simInfo": "T-Mobile",
        "osType": "android",
        "macAddress": "4c:50:77:35:4C:A5",
        "wifiBSSID": "02:00:00:00:00:00",
        "wifiSSID": "<unknown ssid>",
        "imsiMd5": "04bb1f9a2ae7b9d0eba3500795cae686",
        "imei": "865881031117358",
        "apn": "wifi"
    }
}
        "#;

        let mirai_device: MiraiDeviceInfo = serde_json::from_str(mirai_device).unwrap();
        println!("{:#?}", mirai_device);
    }
}

struct CliLogger;

impl Subscriber for CliLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        todo!()
    }

    fn new_span(&self, span: &Attributes<'_>) -> Id {
        todo!()
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        todo!()
    }

    fn record_follows_from(&self, span: &Id, follows: &Id) {
        todo!()
    }

    fn event(&self, event: &Event<'_>) {

        todo!()
    }

    fn enter(&self, span: &Id) {
        todo!()
    }

    fn exit(&self, span: &Id) {
        todo!()
    }
}
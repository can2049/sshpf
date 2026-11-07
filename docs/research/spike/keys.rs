//! Key-format coverage: sshpf users have RSA/ECDSA/ed25519, some encrypted.

use std::process::Command;

fn try_load(path: &str, pass: Option<&str>) -> String {
    match russh::keys::load_secret_key(path, pass) {
        Ok(k) => format!("OK algo={:?} encrypted_ok", k.algorithm()),
        Err(e) => format!("ERR {e}"),
    }
}

fn main() {
    let dir = "/tmp/russh-spike/keys";
    let _ = std::fs::create_dir_all(dir);
    let variants: Vec<(&str, Vec<&str>)> = vec![
        ("ed25519", vec!["-t", "ed25519"]),
        ("rsa2048", vec!["-t", "rsa", "-b", "2048"]),
        ("rsa4096", vec!["-t", "rsa", "-b", "4096"]),
        ("ecdsa256", vec!["-t", "ecdsa", "-b", "256"]),
        ("ecdsa384", vec!["-t", "ecdsa", "-b", "384"]),
        ("ecdsa521", vec!["-t", "ecdsa", "-b", "521"]),
    ];
    println!("== unencrypted ==");
    for (name, args) in &variants {
        let p = format!("{dir}/{name}");
        let _ = std::fs::remove_file(&p);
        let st = Command::new("ssh-keygen").args(["-q", "-f", &p, "-N", "", "-C", name]).args(args).status().unwrap();
        if !st.success() { println!("  {name:10} ssh-keygen failed"); continue; }
        println!("  {name:10} {}", try_load(&p, None));
    }

    println!("\n== encrypted with passphrase ==");
    for (name, args) in [("enc-ed25519", vec!["-t","ed25519"]), ("enc-rsa", vec!["-t","rsa","-b","2048"])] {
        let p = format!("{dir}/{name}");
        let _ = std::fs::remove_file(&p);
        let st = Command::new("ssh-keygen").args(["-q","-f",&p,"-N","hunter2","-C",name]).args(&args).status().unwrap();
        if !st.success() { println!("  {name:10} ssh-keygen failed"); continue; }
        println!("  {name:10} no-pass   -> {}", try_load(&p, None));
        println!("  {name:10} good-pass -> {}", try_load(&p, Some("hunter2")));
        println!("  {name:10} bad-pass  -> {}", try_load(&p, Some("wrong")));
    }

    println!("\n== pkcs8 / pem format ==");
    let p8 = format!("{dir}/pkcs8");
    let _ = Command::new("ssh-keygen").args(["-q","-f",&p8,"-N","","-t","ed25519","-m","PKCS8"]).status();
    println!("  pkcs8      -> {}", try_load(&p8, None));
}

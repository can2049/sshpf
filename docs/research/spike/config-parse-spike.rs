use std::fs::File;
use std::io::BufReader;
use ssh2_config::{ParseRule, SshConfig};

fn main() -> anyhow::Result<()> {
    let path = std::env::var("HOME")? + "/.ssh/config";
    let mut r = BufReader::new(File::open(&path)?);
    let c = SshConfig::default().parse(&mut r, ParseRule::ALLOW_UNKNOWN_FIELDS)?;
    let mut aliases = Vec::new();
    for h in c.get_hosts() {
        for pat in &h.pattern {
            if !pat.pattern.contains('*') && !pat.pattern.contains('?') && !pat.negated {
                aliases.push(pat.pattern.clone());
            }
        }
    }
    aliases.sort();
    aliases.dedup();
    println!("real ~/.ssh/config -> {} concrete aliases", aliases.len());
    let mut with_jump = 0;
    for a in &aliases {
        let p = c.query(a);
        let proxy = p.proxy_jump.map(|v| v.join(",")).unwrap_or_else(|| "-".into());
        if proxy != "-" { with_jump += 1; }
        println!("  {a:28} host={:?} user={:?} port={:?} jump={}",
            p.host_name, p.user, p.port, proxy);
    }
    println!("\n{with_jump} host(s) use ProxyJump");
    Ok(())
}

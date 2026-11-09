//! Assertion 6: the frame codec round-trips, and the size budget holds.
//!
//! What belongs here: proof that the real-sized payload survives encode/decode
//! unchanged, the measured byte counts the protocol document quotes, and the
//! TOML limitations that shaped the schema (verified by trying them, not by
//! assuming).
//!
//! What does not: the message vocabulary (that is `msg`), or anything about
//! processes.
//!
//! Run with `cargo run --bin codec`.

#[path = "common/mod.rs"]
mod common;

use common::assert::{check, finish};
use common::frame::{read_frame, write_frame};
use common::msg::ServerMsg;
use common::wire::Snapshot;

fn main() {
    round_trip();
    println!();
    size_budget();
    println!();
    toml_limits();
    std::process::exit(finish("codec"));
}

/// A 50-tunnel snapshot must survive the full frame path unchanged.
fn round_trip() {
    let original = Snapshot::sample(42, 50);
    let message = ServerMsg::Snapshot(original.clone());

    let mut bytes = Vec::new();
    let payload = match toml::to_string(&message) {
        Ok(p) => p,
        Err(e) => {
            check("codec: encode", false, &e.to_string());
            return;
        }
    };
    if let Err(e) = write_frame(&mut bytes, &payload) {
        check("codec: write frame", false, &e.to_string());
        return;
    }
    let frame_len = bytes.len();

    let back = match read_frame(&mut bytes.as_slice()) {
        Ok(Some(p)) => p,
        Ok(None) => {
            check("codec: read frame", false, "unexpected EOF");
            return;
        }
        Err(e) => {
            check("codec: read frame", false, &e.to_string());
            return;
        }
    };
    let decoded: ServerMsg = match toml::from_str(&back) {
        Ok(m) => m,
        Err(e) => {
            check("codec: decode", false, &e.to_string());
            return;
        }
    };

    let equal = match &decoded {
        ServerMsg::Snapshot(s) => s == &original,
        _ => false,
    };
    println!("50-tunnel snapshot: payload={} bytes, frame={} bytes", payload.len(), frame_len);
    check("codec: a 50-tunnel snapshot round-trips field-for-field", equal, "");
    check(
        "codec: the frame stays far below the 1 MiB limit",
        frame_len < common::frame::MAX_FRAME,
        &format!("{frame_len}B"),
    );
}

/// The numbers the protocol document's size budget rests on.
fn size_budget() {
    let one = toml::to_string(&Snapshot::sample(1, 1)).map(|s| s.len()).unwrap_or(0);
    let fifty = toml::to_string(&Snapshot::sample(1, 50)).map(|s| s.len()).unwrap_or(0);
    let thousand = toml::to_string(&Snapshot::sample(1, 1000)).map(|s| s.len()).unwrap_or(0);
    println!("snapshot sizes: 1 tunnel={one}B, 50={fifty}B, 1000={thousand}B");

    // Measured, not assumed. The exact figure depends on how many fields the
    // real `WireTunnelView` carries: this spike's leaner mirror lands near 9.7 KB
    // while a fuller one (with per-tunnel `reason`/timestamps on every row) was
    // measured at ~19.4 KB. Both are far below the frame limit, which is the
    // claim that matters; the band below therefore covers both schemas.
    check("codec: a 50-tunnel snapshot is ~10-20 KB", (8_000..24_000).contains(&fifty), &format!("{fifty}B"));
    check("codec: 1000 tunnels still fit one frame", thousand < common::frame::MAX_FRAME, &format!("{thousand}B"));
    // Full snapshots at tens of tunnels are cheap enough that "always send
    // everything" beats the desync risk of deltas.
    check("codec: full snapshots are affordable at this scale", fifty < 64 * 1024, &format!("{fifty}B"));
}

/// The TOML facts that constrain the schema — each one actually exercised.
fn toml_limits() {
    // (1) TOML has no null: `None` is an absent field, not `null`.
    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct OneLevel {
        a: u32,
        #[serde(default)]
        b: Option<u64>,
    }
    let encoded = toml::to_string(&OneLevel { a: 1, b: None }).unwrap_or_default();
    let absent = !encoded.contains('b');
    let decoded: Option<OneLevel> = toml::from_str(&encoded).ok();
    check(
        "codec: None is encoded as an absent field (no null in TOML)",
        absent && decoded == Some(OneLevel { a: 1, b: None }),
        &format!("{:?}", encoded.replace('\n', "|")),
    );

    let some_encoded = toml::to_string(&OneLevel { a: 1, b: Some(9) }).unwrap_or_default();
    check(
        "codec: Some round-trips through one level of Option",
        toml::from_str::<OneLevel>(&some_encoded).ok() == Some(OneLevel { a: 1, b: Some(9) }),
        "",
    );

    // (2) Nested Option is LOSSY: Some(None) and None produce identical bytes,
    //     so the protocol forbids it. This is the proof, not an assertion.
    #[derive(serde::Serialize, Debug)]
    struct Nested {
        a: Option<Option<u32>>,
    }
    let some_none = toml::to_string(&Nested { a: Some(None) }).unwrap_or_default();
    let none = toml::to_string(&Nested { a: None }).unwrap_or_default();
    check(
        "codec: nested Option is lossy, which is why the schema forbids it",
        some_none == none,
        &format!("Some(None)={:?} None={:?}", some_none, none),
    );

    // (3) Scalars must precede tables when hand-writing TOML; the serialiser
    //     handles this itself, which is why the protocol never hand-builds text.
    #[derive(serde::Serialize, serde::Deserialize, Debug)]
    struct WithTable {
        rev: u64,
        #[serde(default)]
        items: Vec<Item>,
    }
    #[derive(serde::Serialize, serde::Deserialize, Debug)]
    struct Item {
        id: u64,
    }
    let good = "rev = 7\n[[items]]\nid = 1\n";
    let bad = "[[items]]\nid = 1\n\nrev = 7\n";
    check("codec: a hand-written scalar after a table fails to parse", toml::from_str::<WithTable>(bad).is_err(), "");
    check("codec: a hand-written scalar before a table parses", toml::from_str::<WithTable>(good).is_ok(), "");

    // (4) Unknown fields are ignored by default -> an older client tolerates a
    //     newer server adding fields.
    #[derive(serde::Deserialize, Debug)]
    struct Narrow {
        id: u64,
    }
    let widened = "id = 3\nnew_field = \"added later\"\n";
    check(
        "codec: unknown fields are ignored (forward compatibility)",
        toml::from_str::<Narrow>(widened).map(|n| n.id).ok() == Some(3),
        "",
    );
}

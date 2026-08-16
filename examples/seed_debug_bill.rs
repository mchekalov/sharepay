//! Local-testing-only helper: seeds a bill directly into `pending_confirmation`
//! with hand-entered items, bypassing the Receipt Recognizer entirely. Not
//! part of the app; used to exercise the host-review -> confirm -> QR ->
//! participant-marking -> share-total flow via the real server/routes
//! without depending on OCR quality for a given photo.
//!
//! Usage:
//!   SHAREPAY_DB_PATH=sharepay_debug.db cargo run --example seed_debug_bill

use sharepay::bill;
use sharepay::bill::token::{generate_token, hash_token};
use sharepay::db::{self, DbConfig};
use sharepay::pricing::api::{self, NewItem};

#[tokio::main]
async fn main() {
    let config = DbConfig::from_env();
    let pool = db::init_pool(&config)
        .await
        .expect("failed to initialize database pool / run migrations");

    let bill_id = generate_token();
    let host_token = generate_token();

    api::create_bill(&pool, &bill_id, &hash_token(&host_token))
        .await
        .expect("create_bill failed");

    let items = vec![
        NewItem { name: "Айран (йогурт) 450г".to_string(), price_cents: 52_500 },
        NewItem { name: "Морская капуста маринованная 380г".to_string(), price_cents: 96_500 },
        NewItem { name: "Сыр Willie 180г".to_string(), price_cents: 135_000 },
    ];
    let items_sum: i64 = items.iter().map(|i| i.price_cents).sum();
    api::add_items(&pool, &bill_id, items)
        .await
        .expect("add_items failed");

    let tax_tip_cents = 26_000;
    bill::mark_pending_confirmation(&pool, &bill_id, items_sum + tax_tip_cents, tax_tip_cents)
        .await
        .expect("mark_pending_confirmation failed");

    api::open_bill(&pool, &bill_id)
        .await
        .expect("open_bill failed");

    println!("Seeded bill, already open.");
    println!("bill_id:    {bill_id}");
    println!("host_token: {host_token}");
    println!();
    println!("Host tab (QR/ready screen):");
    println!("  http://localhost:3000/b/{bill_id}/host/{host_token}");
    println!("Participant tab (name entry -> marking UI), open in 2 separate tabs:");
    println!("  http://localhost:3000/b/{bill_id}");
}

//! Manual scale probes for the billing read path. Ignored by default because
//! the 100k-invoice seed is intentionally large; the assertions are generous
//! regression bounds, and the printed numbers are the evidence.
// TEST-SEPARATION: oracle=independent-reference isolation=isolated-data-dir

use std::sync::Arc;
use std::time::Instant;

use faktor_cloud::{ManualClock, OrganizationId};

use crate::authority::InvoiceReconciliationState;
use crate::model::{Invoice, InvoiceLine, InvoiceLineKind, InvoiceStatus};
use crate::service::PaymentService;
use crate::store::{insert_invoice_tx, SqlitePaymentStore};
use crate::{PaymentsConfig, StaticSecrets};

const INVOICES: usize = 100_000;

fn seeded_invoice(org: &str, index: usize, now: i64) -> Invoice {
    Invoice {
        id: format!("inv_scale_{index:06}"),
        organization_id: org.to_string(),
        number: format!("INV-{index:06}"),
        subscription_id: None,
        provider_invoice_id: Some(format!("pinv_scale_{index:06}")),
        provider_idempotency_key: Some(format!("idem_scale_{index:06}")),
        reconciliation: InvoiceReconciliationState::Unreconciled,
        currency: "USD".into(),
        status: InvoiceStatus::Open,
        last_dunning_payment_id: None,
        lines: vec![
            InvoiceLine {
                kind: InvoiceLineKind::Plan,
                description: "bounded plan line".into(),
                quantity: 1,
                unit_amount_minor: 900,
                amount_minor: 900,
            },
            InvoiceLine {
                kind: InvoiceLineKind::Usage,
                description: "bounded usage line".into(),
                quantity: 1,
                unit_amount_minor: 100,
                amount_minor: 100,
            },
        ],
        subtotal_minor: 1_000,
        discount_minor: 0,
        credits_minor: 0,
        tax_minor: 0,
        tax_jurisdiction: None,
        tax_inclusive: false,
        total_minor: 1_000,
        amount_paid_minor: 0,
        amount_refunded_minor: 0,
        period_start_ms: Some(now - 30 * 24 * 60 * 60 * 1_000),
        period_end_ms: Some(now),
        issued_ms: now - i64::try_from(index).unwrap(),
        due_ms: now + 7 * 24 * 60 * 60 * 1_000,
        paid_ms: None,
        voided_ms: None,
        created_ms: now,
    }
}

#[test]
#[ignore = "[perf] 100k-invoice billing page/stats/export/reconciliation bound probe; run explicitly"]
fn perf_100k_invoices_reads_stay_bounded() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("payment.db");
    let store = Arc::new(SqlitePaymentStore::open(&path).unwrap());
    let organization = OrganizationId::try_new("org-scale").unwrap();
    let org = organization.as_str();
    let now = 1_800_000_000_000i64;

    let seed_started = Instant::now();
    store
        .tx(|tx| {
            for index in 0..INVOICES {
                let invoice = seeded_invoice(org, index, now);
                insert_invoice_tx(tx, &invoice, &invoice.lines)?;
            }
            Ok(())
        })
        .unwrap();
    let seed = seed_started.elapsed();

    let stats_started = Instant::now();
    let stats = store.invoice_stats(org).unwrap();
    let stats_elapsed = stats_started.elapsed();

    let page_started = Instant::now();
    let page = store.invoices(org, None, 200).unwrap();
    let page_elapsed = page_started.elapsed();
    assert_eq!(page.items.len(), 200);

    let summary_started = Instant::now();
    let summary = store.reconciliation_summary(org).unwrap();
    let summary_elapsed = summary_started.elapsed();

    let reconcile_started = Instant::now();
    let reconcile = store.invoices_for_reconciliation(org, 5_000).unwrap();
    let reconcile_elapsed = reconcile_started.elapsed();
    assert_eq!(reconcile.len(), 5_000);

    let clock = Arc::new(ManualClock::new(now));
    let service = PaymentService::open_local(
        store.clone(),
        PaymentsConfig::local(),
        clock,
        Arc::new(StaticSecrets::new(std::iter::empty::<(String, String)>())),
    )
    .unwrap();

    let export_started = Instant::now();
    let export = service
        .export_invoices_page(&organization, "csv", None, 5_000)
        .unwrap();
    let export_elapsed = export_started.elapsed();
    assert!(export.len() > 5_000);

    let overview_started = Instant::now();
    let overview = service.overview(&organization).unwrap();
    let overview_elapsed = overview_started.elapsed();
    assert_eq!(overview.invoice_count, INVOICES as u64);

    println!(
        "[perf] 100k invoices seed={seed:?} stats={stats_elapsed:?} (issued={}) page200={page_elapsed:?} \
         reconciliation_summary={summary_elapsed:?} (unreconciled={}) reconcile_page5000={reconcile_elapsed:?} \
         csv5000={export_elapsed:?} overview={overview_elapsed:?}",
        stats.invoice_count,
        summary.unreconciled
            + summary.matched
            + summary.amount_mismatch
            + summary.currency_mismatch
            + summary.provider_invoice_missing
    );
    assert!(
        stats_elapsed.as_millis() < 5_000,
        "stats scan is superlinear"
    );
    assert!(page_elapsed.as_millis() < 1_000, "page read is not indexed");
    assert!(
        reconcile_elapsed.as_millis() < 5_000,
        "reconciliation page is unbounded"
    );
    assert!(
        export_elapsed.as_millis() < 5_000,
        "bounded export exceeded its budget"
    );
    assert!(
        overview_elapsed.as_millis() < 10_000,
        "overview did three unbounded scans"
    );
}

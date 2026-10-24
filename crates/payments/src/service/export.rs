//! Bounded invoice CSV projections (split out of the service core).

use super::*;

pub(super) fn invoices_csv(invoices: &[Invoice]) -> String {
    let mut text = String::from(
        "number,invoice_id,status,currency,line_kind,description,quantity,unit_amount_minor,\
         amount_minor,subtotal_minor,discount_minor,credits_minor,tax_minor,total_minor,\
         amount_paid_minor,amount_refunded_minor,balance_minor,issued_ms,due_ms,\
         tax_jurisdiction,tax_inclusive\n",
    );
    for invoice in invoices {
        for line in &invoice.lines {
            let fields = vec![
                invoice.number.clone(),
                invoice.id.clone(),
                invoice.status.as_str().to_string(),
                invoice.currency.clone(),
                line.kind.as_str().to_string(),
                line.description.clone(),
                line.quantity.to_string(),
                line.unit_amount_minor.to_string(),
                line.amount_minor.to_string(),
                invoice.subtotal_minor.to_string(),
                invoice.discount_minor.to_string(),
                invoice.credits_minor.to_string(),
                invoice.tax_minor.to_string(),
                invoice.total_minor.to_string(),
                invoice.amount_paid_minor.to_string(),
                invoice.amount_refunded_minor.to_string(),
                invoice.balance_minor().to_string(),
                invoice.issued_ms.to_string(),
                invoice.due_ms.to_string(),
                invoice.tax_jurisdiction.clone().unwrap_or_default(),
                invoice.tax_inclusive.to_string(),
            ];
            text.push_str(
                &fields
                    .iter()
                    .map(|field| faktor_cloud::csv_field(field))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            text.push('\n');
        }
    }
    text
}

pub(super) fn invoice_csv(invoice: &Invoice) -> String {
    invoices_csv(std::slice::from_ref(invoice))
}

// Re-export the config helpers the CLI/server need without leaking the
// service's private fields.

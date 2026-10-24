//! Payment-token ingestion, vault rotation/removal and the ONE provider-token
//! validator shared by the API, CLI and service boundaries.

use super::*;
use crate::vault::MAX_TOKEN_BYTES;

impl PaymentService {
    pub fn add_payment_method(
        &self,
        organization: &OrganizationId,
        request: AddPaymentMethodRequest,
    ) -> Result<PaymentMethodOutcome, PaymentError> {
        self.validate_idempotency(&request.idempotency_key)?;
        validate_payment_token(&request.token)?;
        bounded_text("payment brand", &request.brand, 64)?;
        if request.last4.len() != 4 || !request.last4.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PaymentError::Malformed(
                "last4 must be exactly four digits".into(),
            ));
        }
        if !(1..=12).contains(&request.exp_month) {
            return Err(PaymentError::Malformed("exp_month must be 1..=12".into()));
        }
        if !(2000..=2100).contains(&request.exp_year) {
            return Err(PaymentError::Malformed(
                "exp_year must be 2000..=2100".into(),
            ));
        }
        let org = organization.as_str();
        let hash = request_hash(&request)?;
        if let Some(response) =
            self.replay("add_payment_method", org, &request.idempotency_key, &hash)?
        {
            let method = self.method_from_replay(org, &response)?;
            return Ok(PaymentMethodOutcome {
                payment_method: method,
                replayed: true,
            });
        }
        let now = self.now_ms();
        let existing = self.store.payment_methods(org)?;
        let is_default = request.set_default || existing.is_empty();
        let method_id = format!("pm_{}", uuid::Uuid::new_v4());
        let token_ciphertext = self.vault.seal(org, &method_id, &request.token)?;
        let method = StoredPaymentMethod {
            method: PaymentMethod {
                id: method_id,
                organization_id: org.to_string(),
                provider: self.provider.name().to_string(),
                brand: request.brand,
                last4: request.last4,
                exp_month: request.exp_month,
                exp_year: request.exp_year,
                is_default,
                created_ms: now,
            },
            token_ciphertext,
        };
        let write = self.write(
            org,
            &request.idempotency_key,
            "add_payment_method",
            hash.clone(),
            &serde_json::json!({"payment_method_id": method.method.id}),
            now,
        )?;
        let cloned = method.clone();
        match self
            .store
            .record_atomic(&write, |tx| put_payment_method_tx(tx, &cloned))
        {
            Ok(()) => Ok(PaymentMethodOutcome {
                payment_method: PaymentMethodView::from(&method),
                replayed: false,
            }),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response =
                    self.replay("add_payment_method", org, &request.idempotency_key, &hash)?;
                let method = response
                    .ok_or_else(|| {
                        PaymentError::Conflict("payment method was recorded concurrently".into())
                    })
                    .and_then(|response| self.method_from_replay(org, &response))?;
                Ok(PaymentMethodOutcome {
                    payment_method: method,
                    replayed: true,
                })
            }
            Err(e) => Err(e),
        }
    }

    pub fn payment_methods(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<PaymentMethodView>, PaymentError> {
        Ok(self
            .store
            .payment_methods(organization.as_str())?
            .iter()
            .map(PaymentMethodView::from)
            .collect())
    }

    /// Rotate the provider token of one stored method. The old ciphertext is
    /// replaced atomically; the plaintext token never leaves this call.
    pub fn rotate_payment_method_token(
        &self,
        organization: &OrganizationId,
        payment_method_id: &str,
        token: &str,
        idempotency_key: &str,
    ) -> Result<PaymentMethodOutcome, PaymentError> {
        self.validate_idempotency(idempotency_key)?;
        validate_payment_token(token)?;
        let org = organization.as_str();
        let existing = self
            .store
            .payment_method(org, payment_method_id)?
            .ok_or_else(|| PaymentError::NotFound("payment method".into()))?;
        if existing.method.provider != self.provider.name() {
            return Err(PaymentError::Malformed(
                "the payment method belongs to a different payment provider".into(),
            ));
        }
        let hash = request_hash(&serde_json::json!({
            "payment_method_id": payment_method_id,
            "token_digest": sha256_hex(token.as_bytes()),
        }))?;
        let operation = "rotate_payment_method_token";
        if let Some(response) = self.replay(operation, org, idempotency_key, &hash)? {
            return Ok(PaymentMethodOutcome {
                payment_method: self.method_from_replay(org, &response)?,
                replayed: true,
            });
        }
        let mut updated = existing;
        updated.token_ciphertext = self.vault.seal(org, payment_method_id, token)?;
        let now = self.now_ms();
        let write = self.write(
            org,
            idempotency_key,
            operation,
            hash.clone(),
            &serde_json::json!({"payment_method_id": payment_method_id}),
            now,
        )?;
        let cloned = updated.clone();
        match self
            .store
            .record_atomic(&write, |tx| put_payment_method_tx(tx, &cloned))
        {
            Ok(()) => Ok(PaymentMethodOutcome {
                payment_method: PaymentMethodView::from(&updated),
                replayed: false,
            }),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response = self
                    .replay(operation, org, idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict(
                            "payment method rotation was recorded concurrently".into(),
                        )
                    })?;
                Ok(PaymentMethodOutcome {
                    payment_method: self.method_from_replay(org, &response)?,
                    replayed: true,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Remove one stored token. The ciphertext row is deleted (not merely
    /// hidden), and a removed default is replaced by the oldest remaining
    /// method so the organization never has a dangling default.
    pub fn remove_payment_method(
        &self,
        organization: &OrganizationId,
        payment_method_id: &str,
        idempotency_key: &str,
    ) -> Result<bool, PaymentError> {
        self.validate_idempotency(idempotency_key)?;
        let org = organization.as_str();
        let existing = self
            .store
            .payment_method(org, payment_method_id)?
            .ok_or_else(|| PaymentError::NotFound("payment method".into()))?;
        let hash = request_hash(&serde_json::json!({"payment_method_id": payment_method_id}))?;
        let operation = "remove_payment_method";
        if self
            .replay(operation, org, idempotency_key, &hash)?
            .is_some()
        {
            return Ok(false);
        }
        let now = self.now_ms();
        let remaining: Vec<StoredPaymentMethod> = self
            .store
            .payment_methods(org)?
            .into_iter()
            .filter(|method| method.method.id != payment_method_id)
            .collect();
        let replacement = if existing.method.is_default {
            remaining.first().cloned().map(|mut method| {
                method.method.is_default = true;
                method
            })
        } else {
            None
        };
        let write = self.write(
            org,
            idempotency_key,
            operation,
            hash,
            &serde_json::json!({"payment_method_id": payment_method_id, "removed": true}),
            now,
        )?;
        match self.store.record_atomic(&write, |tx| {
            tx.execute(
                "DELETE FROM payment_method WHERE organization_id = ?1 AND id = ?2",
                rusqlite::params![org, payment_method_id],
            )
            .map_err(|e| PaymentError::Store(format!("payment store: {e}")))?;
            if let Some(replacement) = &replacement {
                put_payment_method_tx(tx, replacement)?;
            }
            Ok(true)
        }) {
            Ok(removed) => Ok(removed),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub(super) fn migrate_plaintext_payment_tokens(&self) -> Result<(), PaymentError> {
        for (id, organization_id, payload) in self.store.raw_payment_methods()? {
            let mut value: serde_json::Value = serde_json::from_str(&payload)
                .map_err(|e| PaymentError::Store(format!("payment method row decoding: {e}")))?;
            let object = value.as_object_mut().ok_or_else(|| {
                PaymentError::Store("payment method row is not a JSON object".into())
            })?;
            let legacy = object.remove("token");
            match (legacy, object.get("token_ciphertext").is_some()) {
                (Some(_), true) => {
                    return Err(PaymentError::Store(
                        "payment method row carries both a plaintext token and ciphertext".into(),
                    ))
                }
                (Some(serde_json::Value::String(token)), false) => {
                    if let Err(error) = validate_payment_token(&token) {
                        // A legacy raw-PAN row is not migratable and must not
                        // survive as plaintext: delete it, then fail closed.
                        self.store.remove_payment_method(&organization_id, &id)?;
                        return Err(error);
                    }
                    let sealed = self.vault.seal(&organization_id, &id, &token)?;
                    object.insert("token_ciphertext".into(), serde_json::Value::String(sealed));
                    self.store
                        .rewrite_payment_method_payload(&id, &organization_id, &value)?;
                }
                (Some(_), false) => {
                    return Err(PaymentError::Store(
                        "legacy payment method token is not a string".into(),
                    ))
                }
                (None, true) => {
                    let sealed = object
                        .get("token_ciphertext")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if !sealed.starts_with("v1:") {
                        return Err(PaymentError::Store(
                            "payment method row carries a non-vault ciphertext".into(),
                        ));
                    }
                }
                (None, false) => {
                    return Err(PaymentError::Store(
                        "payment method row carries no token material".into(),
                    ))
                }
            }
        }
        Ok(())
    }

    pub(super) fn payment_method_token(
        &self,
        organization_id: &str,
        payment_method_id: &str,
    ) -> Result<crate::secrets::SecretString, PaymentError> {
        let stored = self
            .store
            .payment_method(organization_id, payment_method_id)?
            .ok_or_else(|| PaymentError::NotFound("payment method".into()))?;
        self.vault
            .open(organization_id, payment_method_id, &stored.token_ciphertext)
    }
}

pub fn validate_payment_token(token: &str) -> Result<(), PaymentError> {
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
        return Err(PaymentError::Malformed(format!(
            "payment token must be 1..={MAX_TOKEN_BYTES} bytes"
        )));
    }
    if !token.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
    }) {
        return Err(PaymentError::Malformed(
            "payment token must be an opaque provider token (letters, digits, _-.:/ only)".into(),
        ));
    }
    let digits: Vec<u8> = token.bytes().filter(u8::is_ascii_digit).collect();
    if (12..=19).contains(&digits.len()) && luhn_valid(&digits) {
        return Err(PaymentError::Malformed(
            "raw card numbers are never accepted; use a provider token".into(),
        ));
    }
    // A contiguous 12..=19 digit run is card-like even when its checksum is
    // wrong (a mistyped/forged PAN must not become a stored token either).
    if token
        .split(|c: char| !c.is_ascii_digit())
        .any(|run| (12..=19).contains(&run.len()))
    {
        return Err(PaymentError::Malformed(
            "raw card numbers are never accepted; use a provider token".into(),
        ));
    }
    Ok(())
}

fn luhn_valid(digits: &[u8]) -> bool {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let mut sum = 0u32;
    let parity = digits.len() % 2;
    for (index, digit) in digits.iter().enumerate() {
        let mut value = u32::from(digit - b'0');
        if index % 2 == parity {
            value *= 2;
            if value > 9 {
                value -= 9;
            }
        }
        sum += value;
    }
    sum.is_multiple_of(10)
}

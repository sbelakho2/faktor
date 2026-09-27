//! `runtime::media_tests`: out-of-line tests.

#![allow(unused_imports)]

use super::*;
use crate::runtime::fixtures_tests::*;
use crate::runtime::tests::*;
use crate::*;

/// Request construction (media): durable `AttachmentId`s resolve to
/// bounded in-memory `ImageData`/`FileData` parts at request time —
/// order preserved, opaque non-documents excluded — and NO expanded byte
/// ever reaches durable state (the task row keeps ids only).
#[tokio::test]
async fn attachment_media_reaches_the_request_byte_exact_and_never_persists() {
    fn vision_caps() -> ModelCapabilities {
        ModelCapabilities {
            vision: true,
            // A realistic media-capable window: the resolved PDF part
            // carries a fixed conservative token estimate, and document
            // turns must never spuriously trigger compaction in this
            // delivery test.
            context: 131_072,
            ..Default::default()
        }
    }
    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
    let jpg: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE0, 9, 8, 7];
    let pdf: Vec<u8> = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF".to_vec();
    let captured: Arc<std::sync::Mutex<Vec<Vec<u8>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_hook = captured.clone();
    let expected_png = png.clone();
    let expected_jpg = jpg.clone();
    let expected_pdf = pdf.clone();
    let wrapper = Arc::new(InspectingProvider::new(
        Arc::new(
            FakeProvider::with_script(
                "fake",
                vision_caps(),
                vec![ScriptedResponse::Text("seen".into()), ScriptedResponse::End],
            )
            .with_documents(),
        ),
        move |_n, req| {
            let mut media = Vec::new();
            let mut docs = Vec::new();
            for m in &req.messages {
                for p in &m.content {
                    match &p.kind {
                        faktor_provider::ContentKind::ImageData { mime, data } => {
                            assert!(
                                mime == "image/png" || mime == "image/jpeg",
                                "unexpected mime {mime}"
                            );
                            media.push((mime.clone(), data.as_slice().to_vec()));
                        }
                        faktor_provider::ContentKind::FileData { mime, data, .. } => {
                            assert_eq!(mime, "application/pdf");
                            docs.push(data.as_slice().to_vec());
                        }
                        _ => {}
                    }
                }
            }
            if media.len() != 2 {
                return Err(format!("expected 2 resolved images, got {}", media.len()));
            }
            if docs.len() != 1 || docs[0] != expected_pdf {
                return Err("the resolved PDF is not byte-exact".into());
            }
            if media[0].0 != "image/png" || media[0].1 != expected_png {
                return Err("first image is not the byte-exact PNG".into());
            }
            if media[1].0 != "image/jpeg" || media[1].1 != expected_jpg {
                return Err("second image is not the byte-exact JPEG (order lost?)".into());
            }
            *captured_hook.lock().unwrap() = media.into_iter().map(|(_, b)| b).collect();
            Ok(())
        },
    ));
    let (deps, _dir) = deps_with(wrapper, vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let img1 = handle
        .put_attachment("image/png", Some("a.png"), &png)
        .unwrap();
    let img2 = handle
        .put_attachment("image/jpeg", Some("b.jpg"), &jpg)
        .unwrap();
    let pdf_id = handle
        .put_attachment("application/pdf", Some("spec.pdf"), &pdf)
        .unwrap();
    runtime
        .seed_task_attachments(session, &[img1.clone(), img2.clone(), pdf_id.clone()])
        .unwrap();
    let outcome = runtime
        .run_turn(session, "describe both images and the spec", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(
        captured.lock().unwrap().as_slice(),
        &[png.clone(), jpg.clone()][..],
        "provider saw the byte-exact ordered images"
    );
    // Durable state keeps the typed set ONLY: no expanded bytes and no
    // base64 anywhere in the task row or the message rows.
    let task = handle.get_task(handle.task_id().unwrap()).unwrap().unwrap();
    assert_eq!(task.attachments, vec![img1, img2, pdf_id]);
    let page = handle.messages_page(None, 50).unwrap();
    let durable_json = serde_json::to_string(
        &page
            .messages
            .iter()
            .map(|m| serde_json::to_value(&m.parts).unwrap())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    for secret in [
        faktor_provider::MediaBytes::new(png.clone())
            .unwrap()
            .to_base64(),
        faktor_provider::MediaBytes::new(jpg.clone())
            .unwrap()
            .to_base64(),
        faktor_provider::MediaBytes::new(pdf.clone())
            .unwrap()
            .to_base64(),
    ] {
        assert!(
            !durable_json.contains(&secret),
            "base64 media leaked into durable message state"
        );
    }
    assert!(!durable_json.contains("0x89") && !durable_json.contains("\\u0089"));
}

/// A vision-less selected model refuses the turn TYPEDLY before any
/// provider call and keeps the durable bytes intact.
#[tokio::test]
async fn attachment_media_visionless_model_is_a_typed_refusal_with_bytes_kept() {
    let (deps, _dir) = deps(
        FakeProvider::with_script(
            "fake",
            ModelCapabilities::default(),
            vec![ScriptedResponse::End],
        ),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let image = handle
        .put_attachment("image/png", Some("a.png"), b"\x89PNG")
        .unwrap();
    runtime
        .seed_task_attachments(session, std::slice::from_ref(&image))
        .unwrap();
    let provider = runtime.deps.providers.get("fake").unwrap();
    let caps = provider.capabilities("m");
    assert!(!caps.vision);
    let mut history = vec![RequestMessage {
        role: Role::User,
        content: vec![ContentPart::text("what is this?")],
    }];
    let err = runtime
        .inject_attachment_media(&handle, &mut history, &caps, provider.as_ref())
        .expect_err("vision-less model must refuse");
    assert!(err.message.contains("does not support vision"), "{err:?}");
    assert_eq!(err.kind, ErrorKind::Malformed);
    // The draft-equivalent durable bytes and row remain intact.
    assert_eq!(
        handle.attachment_bytes(&image, 1 << 20).unwrap(),
        b"\x89PNG"
    );
    assert_eq!(handle.list_attachments(16).unwrap(), vec![image]);
}

/// Router qualification (P1 item 10): a turn carrying resolved image
/// parts requires `vision` on the routed call, so a vision-less
/// candidate can never win the consult and fail only at the provider
/// boundary; a text-only turn's consult and wire plan are untouched.
#[tokio::test]
async fn routed_consult_requires_vision_when_images_ride_the_turn() {
    fn vision_caps() -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            streaming: true,
            vision: true,
            context: 131_072,
            ..Default::default()
        }
    }
    type WireCapture = Vec<RequestMessage>;
    let seen: Arc<std::sync::Mutex<Vec<WireCapture>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let spy = RecordingRouter::new();
    let inner = Arc::new(FakeProvider::with_script(
        "fake",
        vision_caps(),
        vec![ScriptedResponse::Text("seen".into()), ScriptedResponse::End],
    ));
    let provider = Arc::new(InspectingProvider::new(inner, move |_n, req| {
        sink.lock().unwrap().push(req.messages.clone());
        Ok(())
    }));
    let (mut deps, _dir) = deps_with(provider, vec![]);
    deps.routing = spy.clone();
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let image = handle
        .put_attachment("image/png", Some("a.png"), b"\x89PNG")
        .unwrap();
    runtime
        .seed_task_attachments(session, std::slice::from_ref(&image))
        .unwrap();
    let outcome = runtime
        .run_turn(session, "describe the image", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let requests = spy.requests();
    let implement = requests
        .iter()
        .find(|r| r.phase == RouterPhase::Implement)
        .expect("the image turn must route the Implement call");
    assert!(
        implement
            .required_capabilities
            .iter()
            .any(|c| c == "vision"),
        "image parts must require vision at qualification: {implement:?}"
    );
    // The wire order is text FIRST, the byte-exact image appended after
    // (ordered content parts, never a replaced text blob).
    let wires = seen.lock().unwrap().clone();
    assert_eq!(wires.len(), 1, "one provider request");
    let user = wires[0]
            .iter()
            .rev()
            .find(|m| {
                m.role == Role::User
                    && m.content.iter().any(|p| {
                        matches!(&p.kind, faktor_provider::ContentKind::Text { text } if !text.is_empty())
                    })
            })
            .expect("the turn's user message");
    assert!(
        matches!(
            &user.content[0].kind,
            faktor_provider::ContentKind::Text { text } if text == "describe the image"
        ),
        "the prompt text part must stay first and byte-identical"
    );
    assert!(
        matches!(
            &user.content[1].kind,
            faktor_provider::ContentKind::ImageData { mime, data }
                if mime.as_str() == "image/png" && data.as_slice() == b"\x89PNG"
        ),
        "the byte-exact image must follow the text part"
    );

    // Text-only turn on the same graph: no vision requirement and no
    // media part anywhere on the wire (the text-only plan is untouched).
    let seen: Arc<std::sync::Mutex<Vec<WireCapture>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let spy = RecordingRouter::new();
    let inner = Arc::new(FakeProvider::with_script(
        "fake",
        vision_caps(),
        vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
    ));
    let provider = Arc::new(InspectingProvider::new(inner, move |_n, req| {
        sink.lock().unwrap().push(req.messages.clone());
        Ok(())
    }));
    let (mut deps, _dir2) = deps_with(provider, vec![]);
    deps.routing = spy.clone();
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "hello", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    for r in spy.requests() {
        assert!(
            !r.required_capabilities.iter().any(|c| c == "vision"),
            "text-only consults must not require vision: {r:?}"
        );
    }
    let wires = seen.lock().unwrap().clone();
    assert_eq!(wires.len(), 1, "one provider request");
    for m in &wires[0] {
        for p in &m.content {
            assert!(
                matches!(&p.kind, faktor_provider::ContentKind::Text { .. }),
                "text-only wire carries no media parts: {p:?}"
            );
        }
    }
}

/// A document-capable provider receives non-image DOCUMENT attachments
/// as byte-exact `FileData` parts (PDF and plain text, input order
/// preserved), while opaque non-document attachments stay CAS-only.
#[tokio::test]
async fn attachment_documents_reach_the_request_byte_exact_for_document_capable_providers() {
    let pdf: Vec<u8> = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF".to_vec();
    let text: Vec<u8> = b"plain notes\n".to_vec();
    let fake = Arc::new(
        FakeProvider::with_script(
            "fake",
            ModelCapabilities::default(),
            vec![ScriptedResponse::End],
        )
        .with_documents(),
    );
    let (deps, _dir) = deps_with(fake, vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let pdf_id = handle
        .put_attachment("application/pdf", Some("spec.pdf"), &pdf)
        .unwrap();
    let text_id = handle
        .put_attachment("text/plain", Some("notes.txt"), &text)
        .unwrap();
    let opaque = handle
        .put_attachment("application/zip", Some("bundle.zip"), b"PK\x03\x04")
        .unwrap();
    runtime
        .seed_task_attachments(session, &[pdf_id.clone(), text_id.clone(), opaque.clone()])
        .unwrap();
    let provider = runtime.deps.providers.get("fake").unwrap();
    let caps = provider.capabilities("m");
    let mut history = vec![RequestMessage {
        role: Role::User,
        content: vec![ContentPart::text("read the spec and notes")],
    }];
    runtime
        .inject_attachment_media(&handle, &mut history, &caps, provider.as_ref())
        .unwrap();
    let docs: Vec<(&str, Option<&str>, &[u8])> = history
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|p| match &p.kind {
            faktor_provider::ContentKind::FileData {
                mime,
                filename,
                data,
            } => Some((mime.as_str(), filename.as_deref(), data.as_slice())),
            _ => None,
        })
        .collect();
    assert_eq!(docs.len(), 2, "PDF + text, in input order: {docs:?}");
    assert_eq!(
        docs[0],
        ("application/pdf", Some("spec.pdf"), pdf.as_slice())
    );
    assert_eq!(docs[1], ("text/plain", Some("notes.txt"), text.as_slice()));
    // The opaque archive never became model content.
    assert!(
        !history.iter().flat_map(|m| &m.content).any(|p| matches!(
            &p.kind,
            faktor_provider::ContentKind::FileData { mime, .. } if mime == "application/zip"
        )),
        "non-document attachments must stay CAS-only"
    );
    // Durable bytes/rows are untouched by delivery.
    assert_eq!(handle.attachment_bytes(&pdf_id, 1 << 20).unwrap(), pdf);
    assert_eq!(handle.list_attachments(16).unwrap().len(), 3);
}

/// A document-less provider refuses the turn TYPEDLY before any provider
/// call and keeps the durable bytes and rows intact ("bytes/draft
/// remain") — never a silent drop and never an invented extraction.
#[tokio::test]
async fn attachment_documentless_model_is_a_typed_refusal_with_bytes_kept() {
    let pdf: Vec<u8> = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF".to_vec();
    let (deps, _dir) = deps(
        FakeProvider::with_script(
            "fake",
            ModelCapabilities::default(),
            vec![ScriptedResponse::End],
        ),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let doc = handle
        .put_attachment("application/pdf", Some("spec.pdf"), &pdf)
        .unwrap();
    runtime
        .seed_task_attachments(session, std::slice::from_ref(&doc))
        .unwrap();
    let provider = runtime.deps.providers.get("fake").unwrap();
    assert!(!provider.document_capable("m"));
    let caps = provider.capabilities("m");
    let mut history = vec![RequestMessage {
        role: Role::User,
        content: vec![ContentPart::text("read the spec")],
    }];
    let err = runtime
        .inject_attachment_media(&handle, &mut history, &caps, provider.as_ref())
        .expect_err("document-less provider must refuse");
    assert_eq!(err.kind, ErrorKind::Malformed);
    assert!(
        err.message.contains("does not support document input"),
        "{err:?}"
    );
    // Nothing was partially injected and the draft-equivalent durable
    // bytes and row remain intact.
    assert!(
        !history
            .iter()
            .flat_map(|m| &m.content)
            .any(|p| matches!(p.kind, faktor_provider::ContentKind::FileData { .. })),
        "no partial document may survive a refused admission"
    );
    assert_eq!(handle.attachment_bytes(&doc, 1 << 20).unwrap(), pdf);
    assert_eq!(handle.list_attachments(16).unwrap(), vec![doc]);
}

/// A missing or tampered CAS blob fails the request typedly — never a
/// silent text fallback and never wrong bytes under the digest.
#[tokio::test]
async fn attachment_media_missing_or_tampered_blob_is_typed() {
    for tamper in [false, true] {
        let (deps, _dir) = deps(
            FakeProvider::with_script(
                "fake",
                ModelCapabilities {
                    vision: true,
                    ..Default::default()
                },
                vec![ScriptedResponse::End],
            ),
            vec![],
        );
        let runtime = AgentRuntime::new(deps).unwrap();
        let session = new_session(runtime.deps());
        let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
        let image = handle
            .put_attachment("image/png", Some("a.png"), b"\x89PNG-original")
            .unwrap();
        runtime
            .seed_task_attachments(session, std::slice::from_ref(&image))
            .unwrap();
        let blob = runtime
            .deps
            .session
            .cas()
            .root()
            .join(image.digest.cas_path());
        if tamper {
            std::fs::write(&blob, b"\x89PNG-forged!").unwrap();
        } else {
            std::fs::remove_file(&blob).unwrap();
        }
        let provider = runtime.deps.providers.get("fake").unwrap();
        let caps = provider.capabilities("m");
        let mut history = vec![RequestMessage {
            role: Role::User,
            content: vec![ContentPart::text("q")],
        }];
        let err = runtime
            .inject_attachment_media(&handle, &mut history, &caps, provider.as_ref())
            .expect_err("missing/tampered blob must fail typed");
        assert!(
            matches!(err.kind, ErrorKind::NotFound | ErrorKind::Store),
            "tamper={tamper} => {:?}",
            err.kind
        );
        assert!(
            !history
                .iter()
                .flat_map(|m| &m.content)
                .any(|p| matches!(p.kind, faktor_provider::ContentKind::ImageData { .. })),
            "no partial media may survive a failed resolution"
        );
    }
}

/// Re-attach reconstructs the same request bytes from durable state
/// alone: two independent constructions produce byte-identical
/// histories (serde-JSON equal), and a second resolution of the same
/// CAS bytes is stable.
#[tokio::test]
async fn attachment_media_reattach_reconstructs_identical_request_bytes() {
    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 4, 5, 6];
    let (deps, _dir) = deps(
        FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                vision: true,
                ..Default::default()
            },
            vec![ScriptedResponse::End],
        ),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let image = handle
        .put_attachment("image/png", Some("a.png"), &png)
        .unwrap();
    runtime
        .seed_task_attachments(session, std::slice::from_ref(&image))
        .unwrap();
    let provider = runtime.deps.providers.get("fake").unwrap();
    let caps = provider.capabilities("m");
    let build = || {
        let mut history = vec![RequestMessage {
            role: Role::User,
            content: vec![ContentPart::text("look")],
        }];
        runtime
            .inject_attachment_media(&handle, &mut history, &caps, provider.as_ref())
            .unwrap();
        history
    };
    let first = build();
    let second = build();
    assert_eq!(first, second, "re-attach must reconstruct equal requests");
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap(),
        "the JSON projections (digest+size media carriers) are equal too"
    );
    match &first[0].content[1].kind {
        faktor_provider::ContentKind::ImageData { mime, data } => {
            assert_eq!(mime, "image/png");
            assert_eq!(data.as_slice(), &png);
        }
        other => panic!("expected resolved image, got {other:?}"),
    }
}

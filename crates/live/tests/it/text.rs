//! Text bodies as YText: round trips, edit replay, convergence and hostile attributes.

use craft_collab::yrs::updates::decoder::Decode;
use craft_collab::yrs::{Any, ClientID, Doc, GetString, OffsetKind, Options, ReadTxn, StateVector, Text, TextRef, Transact, Update};
use deckcraft_engine::Session;
use deckcraft_live::text::{TextOp, apply_ops, edit_ops, read_body, write_body};
use deckcraft_model::text::{Align, Bullet, ParaProps, Paragraph, Run, RunKind, RunProps, TextBody};
use deckcraft_model::{Presentation, edit, walk};
use proptest::prelude::*;

fn doc_with(client: u64, kind: OffsetKind) -> Doc {
    let mut o = Options::with_client_id(ClientID::new(client));
    o.offset_kind = kind;
    o.cleanup_formatting = false;
    Doc::with_options(o)
}

fn doc(client: u64) -> Doc {
    doc_with(client, OffsetKind::Bytes)
}

fn text_of(d: &Doc) -> TextRef {
    d.get_or_insert_text("t")
}

/// The body's paragraphs with runs merged and empty text runs dropped: the one form a YText
/// can hold.
fn canon(ps: &[Paragraph]) -> Vec<Paragraph> {
    ps.iter()
        .cloned()
        .map(|mut p| {
            p.normalize();
            p
        })
        .collect()
}

fn seeded(client: u64, kind: OffsetKind, body: &TextBody) -> (Doc, TextRef) {
    let d = doc_with(client, kind);
    let t = text_of(&d);
    {
        let mut txn = d.transact_mut();
        write_body(&mut txn, &t, body);
    }
    (d, t)
}

fn read(d: &Doc, t: &TextRef) -> Vec<Paragraph> {
    read_body(&d.transact(), t)
}

fn round_trip(body: &TextBody) -> Vec<Paragraph> {
    let (d, t) = seeded(0, OffsetKind::Bytes, body);
    read(&d, &t)
}

fn bodies_of(p: &Presentation, out: &mut Vec<TextBody>) {
    for slide in &p.slides {
        out.push(slide.notes.clone());
        walk(&slide.shapes, &mut |s, _| {
            if let Some(t) = &s.text {
                out.push(t.clone());
            }
        });
    }
}

/// A body with every kind of content a paragraph holds.
fn rich_body() -> TextBody {
    let bold = RunProps { bold: Some(true), size: Some(28.0), font: Some("+mj-lt".into()), ..Default::default() };
    let link = RunProps {
        italic: Some(true),
        underline: Some("sng".into()),
        link: Some(deckcraft_model::text::Hyperlink {
            action: deckcraft_model::text::Action::Url { url: "https://example.org".into() },
            tooltip: "t".into(),
            highlight_click: false,
            sound: None,
            stop_sound: false,
            zoom_return: None,
        }),
        ..Default::default()
    };
    TextBody {
        paragraphs: vec![
            Paragraph {
                level: 0,
                props: ParaProps { align: Some(Align::Center), ..Default::default() },
                runs: vec![Run::with("Title ", bold.clone()), Run::new("é😀漢字")],
                def_props: RunProps { size: Some(18.0), ..Default::default() },
                end_props: bold.clone(),
            },
            Paragraph {
                level: 2,
                props: ParaProps { bullet: Some(Bullet::Char { char: "•".into() }), margin_left: Some(36.0), ..Default::default() },
                runs: vec![
                    Run::new("before"),
                    Run { text: String::new(), props: link.clone(), kind: RunKind::Break },
                    Run::with("linked", link.clone()),
                    Run { text: "3".into(), props: RunProps::default(), kind: RunKind::Field { field: "slidenum".into() } },
                    Run {
                        text: "x+1".into(),
                        props: bold.clone(),
                        kind: RunKind::Math { omml: "<m:oMath><m:r><m:t>x+1</m:t></m:r></m:oMath>".into() },
                    },
                    Run::new(" after"),
                ],
                ..Default::default()
            },
            Paragraph { end_props: link, ..Default::default() },
            Paragraph { level: 8, runs: vec![Run { text: String::new(), props: RunProps::default(), kind: RunKind::Break }], ..Default::default() },
        ],
        ..Default::default()
    }
}

#[test]
fn every_sample_body_round_trips() {
    let mut bodies = vec![rich_body(), TextBody::default(), TextBody::from_text("")];
    let mut s = Session::new();
    deckcraft_engine::sample::open_sample(&mut s).expect("sample deck");
    bodies_of(&s.doc().expect("sample open").doc, &mut bodies);
    for starter in deckcraft_engine::templates::starters() {
        let p = deckcraft_engine::templates::build(starter.name, deckcraft_model::defaults::WIDE).expect("starter");
        bodies_of(&p, &mut bodies);
    }
    assert!(bodies.len() > 10, "the samples hold text");
    for body in &bodies {
        assert_eq!(round_trip(body), canon(&body.paragraphs), "{body:?}");
    }
}

#[test]
fn one_empty_paragraph_is_one_mark() {
    let (d, t) = seeded(0, OffsetKind::Bytes, &TextBody::from_text(""));
    assert_eq!(t.len(&d.transact()), 1);
}

/// A body made only of scalar run attributes, so equal items also mean equal bytes.
fn scalar_body() -> TextBody {
    let p = RunProps {
        bold: Some(true),
        italic: Some(false),
        size: Some(12.5),
        font: Some("Inter".into()),
        underline: Some("dbl".into()),
        ..Default::default()
    };
    let q = RunProps { kern: Some(0.0), spacing: Some(1.0), lang: Some("fr-FR".into()), no_proof: Some(true), ..Default::default() };
    TextBody {
        paragraphs: vec![
            Paragraph { level: 1, runs: vec![Run::with("ab", p.clone()), Run::with("cd", q.clone()), Run::new("e")], ..Default::default() },
            Paragraph { runs: vec![Run::with("fg", q), Run::with("h", p)], ..Default::default() },
        ],
        ..Default::default()
    }
}

#[test]
fn seeding_the_same_body_gives_the_same_bytes() {
    let state = |body: &TextBody| {
        let (d, _) = seeded(0, OffsetKind::Bytes, body);
        d.transact().encode_state_as_update_v1(&StateVector::default())
    };
    let first = state(&scalar_body());
    for _ in 0..16 {
        assert_eq!(state(&scalar_body()), first);
    }
}

fn update_since(d: &Doc, sv: &StateVector) -> Vec<u8> {
    d.transact().encode_state_as_update_v1(sv)
}

fn apply(d: &Doc, update: &[u8]) {
    let u = Update::decode_v1(update).expect("update decodes");
    d.transact_mut().apply_update(u).expect("update applies");
}

/// Replace `t`'s body with `after` through `edit_ops`, and return the update that made.
fn edit_to(d: &Doc, t: &TextRef, after: &[Paragraph]) -> Vec<u8> {
    let before = read(d, t);
    let sv = d.transact().state_vector();
    let ops = edit_ops(&before, after);
    {
        let mut txn = d.transact_mut();
        apply_ops(&mut txn, t, &ops);
    }
    update_since(d, &sv)
}

/// A random edit, as editing commands make it. Numbers are reduced to the body's sizes.
#[derive(Clone, Debug)]
enum Step {
    Type(usize, usize, char),
    Delete(usize, usize, usize),
    DeleteAcross(usize, usize, usize, usize),
    Enter(usize, usize),
    BackspaceAtStart(usize),
    Bold(usize, usize, usize),
    Level(usize, u8),
    Break(usize, usize),
}

fn pick(n: usize, k: usize) -> usize {
    if n == 0 { 0 } else { k % n }
}

fn step(body: &mut TextBody, s: &Step) {
    let np = body.paragraphs.len().max(1);
    let len = |body: &TextBody, p: usize| body.paragraphs.get(p).map_or(0, Paragraph::char_len);
    match *s {
        Step::Type(p, o, c) => {
            let p = pick(np, p);
            edit::insert(body, (p, pick(len(body, p) + 1, o)), &c.to_string(), None);
        }
        Step::Delete(p, o, n) => {
            let p = pick(np, p);
            let o = pick(len(body, p) + 1, o);
            edit::delete(body, (p, o), (p, o + n));
        }
        Step::DeleteAcross(p, o, q, r) => {
            let (p, q) = (pick(np, p), pick(np, q));
            let (a, b) = ((p, pick(len(body, p) + 1, o)), (q, pick(len(body, q) + 1, r)));
            edit::delete(body, a.min(b), a.max(b));
        }
        Step::Enter(p, o) => {
            let p = pick(np, p);
            edit::insert(body, (p, pick(len(body, p) + 1, o)), "\n", None);
        }
        Step::BackspaceAtStart(p) => {
            let p = pick(np, p);
            if p > 0 {
                edit::delete(body, (p - 1, len(body, p - 1)), (p, 0));
            }
        }
        Step::Bold(p, a, b) => {
            let p = pick(np, p);
            let n = len(body, p) + 1;
            let (a, b) = (pick(n, a), pick(n, b));
            edit::format(body, (p, a.min(b)), (p, a.max(b)), &|r: &mut RunProps| r.bold = Some(!r.bold.unwrap_or(false)));
        }
        Step::Level(p, l) => {
            let p = pick(np, p);
            if let Some(para) = body.paragraphs.get_mut(p) {
                para.level = l % 9;
            }
        }
        Step::Break(p, o) => {
            let p = pick(np, p);
            edit::insert(body, (p, pick(len(body, p) + 1, o)), "\u{b}", None);
        }
    }
}

fn arb_step() -> impl Strategy<Value = Step> {
    let n = 0usize..64;
    prop_oneof![
        (n.clone(), n.clone(), prop::sample::select(vec!['a', 'Z', ' ', 'é', '😀', '漢'])).prop_map(|(p, o, c)| Step::Type(p, o, c)),
        (n.clone(), n.clone(), 0usize..6).prop_map(|(p, o, k)| Step::Delete(p, o, k)),
        (n.clone(), n.clone(), n.clone(), n.clone()).prop_map(|(p, o, q, r)| Step::DeleteAcross(p, o, q, r)),
        (n.clone(), n.clone()).prop_map(|(p, o)| Step::Enter(p, o)),
        n.clone().prop_map(Step::BackspaceAtStart),
        (n.clone(), n.clone(), n.clone()).prop_map(|(p, a, b)| Step::Bold(p, a, b)),
        (n.clone(), 0u8..9).prop_map(|(p, l)| Step::Level(p, l)),
        (n.clone(), n).prop_map(|(p, o)| Step::Break(p, o)),
    ]
}

fn arb_props() -> impl Strategy<Value = RunProps> {
    (
        prop::option::of(any::<bool>()),
        prop::option::of(prop::sample::select(vec![10.0, 18.5, 44.0])),
        prop::option::of(prop::sample::select(vec!["Inter", "+mn-lt"])),
    )
        .prop_map(|(bold, size, font)| RunProps { bold, size, font: font.map(str::to_owned), ..Default::default() })
}

fn arb_run() -> impl Strategy<Value = Run> {
    prop_oneof![
        4 => ("[a-cé😀 ]{0,5}", arb_props()).prop_map(|(t, p)| Run::with(t, p)),
        1 => arb_props().prop_map(|props| Run { text: String::new(), props, kind: RunKind::Break }),
        1 => ("[0-9]{0,2}", arb_props()).prop_map(|(text, props)| Run { text, props, kind: RunKind::Field { field: "slidenum".into() } }),
        1 => ("[xy+]{0,3}", arb_props()).prop_map(|(text, props)| Run { text, props, kind: RunKind::Math { omml: "<m:oMath/>".into() } }),
    ]
}

fn arb_paragraph() -> impl Strategy<Value = Paragraph> {
    (
        0u8..9,
        prop::option::of(prop::sample::select(vec![Align::Left, Align::Center, Align::Right])),
        prop::collection::vec(arb_run(), 0..5),
        arb_props(),
        arb_props(),
    )
        .prop_map(|(level, align, runs, def_props, end_props)| Paragraph {
            level,
            props: ParaProps { align, ..Default::default() },
            runs,
            def_props,
            end_props,
        })
}

fn arb_body() -> impl Strategy<Value = TextBody> {
    prop::collection::vec(arb_paragraph(), 1..5).prop_map(|paragraphs| TextBody { paragraphs, ..Default::default() })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn random_bodies_round_trip(body in arb_body()) {
        prop_assert_eq!(round_trip(&body), canon(&body.paragraphs));
    }

    #[test]
    fn edit_ops_replay_to_after(start in arb_body(), steps in prop::collection::vec(arb_step(), 1..12)) {
        for kind in [OffsetKind::Bytes, OffsetKind::Utf16] {
            let (d, t) = seeded(7, kind, &start);
            let mut body = start.clone();
            for s in &steps {
                step(&mut body, s);
                edit_to(&d, &t, &body.paragraphs);
                prop_assert_eq!(read(&d, &t), canon(&body.paragraphs), "after {:?}", s);
            }
        }
    }
}

fn permutations(n: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return vec![vec![]];
    }
    let mut out = Vec::new();
    for rest in permutations(n - 1) {
        for at in 0..=rest.len() {
            let mut p = rest.clone();
            p.insert(at, n - 1);
            out.push(p);
        }
    }
    out
}

fn plain(ps: &[Paragraph]) -> String {
    ps.iter().map(Paragraph::text).collect::<Vec<_>>().join("\n")
}

#[test]
fn concurrent_typing_and_split_converge() {
    let start = TextBody::from_text("Hello world\nSecond line");
    let (seed, _) = seeded(0, OffsetKind::Bytes, &start);
    let state = update_since(&seed, &StateVector::default());
    let peer = |client: u64| {
        let d = doc(client);
        apply(&d, &state);
        let t = text_of(&d);
        (d, t)
    };
    let edits: [(&str, fn(&mut TextBody)); 3] = [
        ("A types", |b| {
            edit::insert(b, (0, 5), " there", None);
        }),
        ("B types", |b| {
            edit::insert(b, (0, 5), "XY", None);
        }),
        ("C splits", |b| {
            edit::insert(b, (0, 3), "\n", None);
        }),
    ];
    let mut updates = Vec::new();
    for (i, (_, f)) in edits.iter().enumerate() {
        let (d, t) = peer(i as u64 + 1);
        let mut body = start.clone();
        f(&mut body);
        updates.push(edit_to(&d, &t, &body.paragraphs));
    }
    let mut results = Vec::new();
    for order in permutations(3) {
        // Every peer receives the others' updates in this order; an observer receives all three.
        for me in 0..4u64 {
            let (d, t) = peer(10 + me);
            if let Some(own) = updates.get(me as usize) {
                apply(&d, own);
            }
            for &i in &order {
                if i as u64 != me {
                    apply(&d, &updates[i]);
                }
            }
            results.push(read(&d, &t));
        }
    }
    let first = results.first().cloned().expect("results");
    assert!(results.iter().all(|r| *r == first), "every peer and order reads equal");
    let text = plain(&first);
    assert!(text.contains(" there") && text.contains("XY"), "both typed texts survive: {text:?}");
    assert_eq!(first.len(), start.paragraphs.len() + 1, "the split made one more paragraph");
    let typed: usize = [" there", "XY"].iter().map(|s| s.chars().count()).sum();
    assert_eq!(text.chars().count(), plain(&start.paragraphs).chars().count() + typed + 1);
}

#[test]
fn the_final_mark_survives_select_all_delete() {
    let start = rich_body();
    let (d, t) = seeded(1, OffsetKind::Bytes, &start);
    let all = t.len(&d.transact());
    {
        let mut txn = d.transact_mut();
        apply_ops(&mut txn, &t, &[TextOp::Delete { at: 0, len: all + 10 }]);
    }
    let left = read(&d, &t);
    assert_eq!(left.len(), 1, "one paragraph is left");
    assert!(left.iter().all(Paragraph::is_empty));

    // Two peers: one selects all and deletes, the other types at the end at the same time.
    let (seed, _) = seeded(0, OffsetKind::Bytes, &start);
    let state = update_since(&seed, &StateVector::default());
    let (a, b) = (doc(2), doc(3));
    apply(&a, &state);
    apply(&b, &state);
    let (ta, tb) = (text_of(&a), text_of(&b));
    let ua = edit_to(&a, &ta, &[Paragraph::default()]);
    let mut typed = start.clone();
    let last = typed.paragraphs.len() - 1;
    let end = typed.paragraphs[last].char_len();
    edit::insert(&mut typed, (last, end), "late", None);
    let ub = edit_to(&b, &tb, &typed.paragraphs);
    apply(&a, &ub);
    apply(&b, &ua);
    let (ra, rb) = (read(&a, &ta), read(&b, &tb));
    assert_eq!(ra, rb);
    assert_eq!(plain(&ra), "late", "the late typing lands in the one paragraph left");
    assert!(ta.get_string(&a.transact()).ends_with('\n'), "the body still ends with its mark");
}

#[test]
fn junk_attributes_read_as_defaults() {
    let body = TextBody {
        paragraphs: vec![Paragraph {
            level: 3,
            props: ParaProps { align: Some(Align::Right), ..Default::default() },
            runs: vec![Run::with("abc", RunProps { bold: Some(true), size: Some(20.0), ..Default::default() })],
            ..Default::default()
        }],
        ..Default::default()
    };
    let (d, t) = seeded(1, OffsetKind::Bytes, &body);
    {
        let mut txn = d.transact_mut();
        apply_ops(
            &mut txn,
            &t,
            &[
                TextOp::Format {
                    at: 0,
                    len: 3,
                    attrs: vec![("r.size".into(), Some(serde_json::json!("huge"))), ("r.italic".into(), Some(serde_json::json!(true)))],
                },
                TextOp::Format {
                    at: 3,
                    len: 1,
                    attrs: vec![("p.level".into(), Some(serde_json::json!("deep"))), ("p.props".into(), Some(serde_json::json!(5)))],
                },
            ],
        );
        // An embed that is neither a field nor an equation, and one that is not a map.
        t.insert_embed(&mut txn, 1, Any::Bool(true));
        t.insert_embed(&mut txn, 1, Any::Map(std::sync::Arc::new(std::collections::HashMap::from([("image".to_owned(), Any::from("x.png"))]))));
    }
    let got = read(&d, &t);
    let want = vec![Paragraph {
        level: 0,
        props: ParaProps::default(),
        runs: vec![Run::with("abc", RunProps { bold: Some(true), italic: Some(true), ..Default::default() })],
        ..Default::default()
    }];
    assert_eq!(got, want);
}

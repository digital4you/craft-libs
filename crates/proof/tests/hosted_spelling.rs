use craft_proof::{dictionary_generation, is_correct_for, packs};

#[test]
fn a_hosted_pack_proofs_its_language_and_keeps_its_notice() {
    let before = dictionary_generation();
    packs::install_spelling_pack("de-CH", b"SET ISO8859-1\n", b"2\nStrasse\nGr\xfcnspecht\n", "Swiss test notice".into()).unwrap();
    assert_ne!(before, dictionary_generation());
    assert!(is_correct_for("Grünspecht", Some("de-CH")));
    assert!(is_correct_for("Strasse", Some("de-CH")));
    assert!(!is_correct_for("Strassse", Some("de-CH")));
    assert!(is_correct_for("house", Some("en-US")));
    assert!(packs::licence_texts().contains(&("de-ch".into(), "Swiss test notice".into())));
    assert!(packs::install_spelling_pack("de-CH", b"", b"", String::new()).is_err());
    packs::reload();
    assert!(!is_correct_for("Strassse", Some("de-CH")));
}

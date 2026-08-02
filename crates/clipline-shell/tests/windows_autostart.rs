#![cfg(windows)]

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use clipline_shell::windows::autostart::{build_autostart_command, WindowsAutostartRegistration};

fn disposable_value_name(case: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    format!("Clipline.Test.{}.{case}.{nonce}", std::process::id())
}

#[test]
fn command_quoting_preserves_spaces_quotes_and_one_autostart_argument() {
    let command =
        build_autostart_command(Path::new(r#"C:\Program Files\Clip "Preview"\clipline.exe"#))
            .expect("build quoted command");

    assert_eq!(
        command,
        r#""C:\Program Files\Clip \"Preview\"\clipline.exe" --autostart"#
    );
    assert_eq!(command.matches(" --autostart").count(), 1);

    let trailing_slash =
        build_autostart_command(Path::new(r"C:\Clipline Preview\")).expect("quote trailing slash");
    assert_eq!(trailing_slash, r#""C:\Clipline Preview\\" --autostart"#);
}

#[test]
fn transaction_restores_absent_valid_and_foreign_values_exactly() {
    let value_name = disposable_value_name("rollback");
    let desired = WindowsAutostartRegistration::new_disposable(
        &value_name,
        Path::new(r"C:\Program Files\Clipline\clipline.exe"),
    )
    .expect("open disposable registration");
    assert_eq!(desired.raw_command().expect("read absent value"), None);

    let from_absent = desired.set_enabled(true).expect("enable from absent");
    assert!(from_absent.enabled());
    assert!(desired.is_enabled().expect("read enabled status"));
    desired
        .rollback(&from_absent)
        .expect("restore absent value");
    assert_eq!(desired.raw_command().expect("read restored absence"), None);

    desired.set_enabled(true).expect("write valid value");
    let exact_valid = desired
        .raw_command()
        .expect("read valid value")
        .expect("valid value present");
    let from_valid = desired.set_enabled(false).expect("disable valid value");
    desired.rollback(&from_valid).expect("restore valid value");
    assert_eq!(
        desired.raw_command().expect("read restored valid value"),
        Some(exact_valid.clone())
    );

    let foreign =
        WindowsAutostartRegistration::new(&value_name, Path::new(r"D:\Foreign App\foreign.exe"))
            .expect("open foreign registration");
    foreign.set_enabled(true).expect("write foreign value");
    let exact_foreign = desired
        .raw_command()
        .expect("read foreign value")
        .expect("foreign value present");
    assert_ne!(exact_foreign, exact_valid);
    assert!(!desired.is_enabled().expect("foreign is not enabled"));

    let from_foreign = desired.set_enabled(true).expect("replace foreign value");
    assert!(desired.is_enabled().expect("desired value is enabled"));
    desired
        .rollback(&from_foreign)
        .expect("restore foreign value");
    assert_eq!(
        desired.raw_command().expect("read restored foreign value"),
        Some(exact_foreign)
    );

    desired.set_enabled(false).expect("remove test value");
    assert_eq!(desired.raw_command().expect("read final absence"), None);
}

#[test]
fn rollback_refuses_to_overwrite_a_concurrent_foreign_change() {
    let value_name = disposable_value_name("concurrent");
    let desired = WindowsAutostartRegistration::new_disposable(
        &value_name,
        Path::new(r"C:\Clipline\clipline.exe"),
    )
    .expect("open disposable registration");
    let foreign =
        WindowsAutostartRegistration::new(&value_name, Path::new(r"D:\Foreign\foreign.exe"))
            .expect("open foreign registration");

    let change = desired.set_enabled(true).expect("enable desired value");
    foreign.set_enabled(true).expect("replace concurrently");
    let foreign_value = desired.raw_command().unwrap();
    let error = desired
        .rollback(&change)
        .expect_err("concurrent value must be preserved");
    assert!(
        error.to_string().contains("changed concurrently"),
        "{error}"
    );
    assert_eq!(desired.raw_command().unwrap(), foreign_value);
}

#[test]
fn disposable_scope_refuses_and_preserves_an_existing_value() {
    let value_name = disposable_value_name("existing");
    let owner = WindowsAutostartRegistration::new_disposable(
        &value_name,
        Path::new(r"C:\Clipline Test Owner\clipline.exe"),
    )
    .expect("open owning disposable registration");
    owner.set_enabled(true).expect("write owned value");
    let before = owner.raw_command().expect("read owned value");

    let error =
        WindowsAutostartRegistration::new_disposable(&value_name, Path::new(r"D:\Other\other.exe"))
            .err()
            .expect("existing value must be rejected");
    assert!(error.to_string().contains("already exists"), "{error}");
    assert_eq!(owner.raw_command().expect("read preserved value"), before);
}

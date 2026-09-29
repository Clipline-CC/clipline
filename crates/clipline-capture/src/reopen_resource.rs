//! Exclusive resources must be released before their replacements can open.
pub(crate) fn reopen_resource<T, E>(
    slot: &mut Option<T>,
    open: impl FnOnce() -> Result<T, E>,
) -> Result<(), E> {
    drop(slot.take());
    *slot = Some(open()?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn releases_exclusive_resource_before_opening_replacement() {
        let old = Rc::new(());
        let weak = Rc::downgrade(&old);
        let mut slot = Some(old);
        reopen_resource(&mut slot, || {
            assert!(
                weak.upgrade().is_none(),
                "old duplication still owns the output"
            );
            Ok::<_, ()>(Rc::new(()))
        })
        .unwrap();
        assert!(slot.is_some());
    }

    #[test]
    fn failed_reopen_leaves_no_stale_resource_and_can_retry() {
        let old = Rc::new(());
        let weak = Rc::downgrade(&old);
        let mut slot = Some(old);
        assert_eq!(
            reopen_resource(&mut slot, || Err("desktop unavailable")),
            Err("desktop unavailable")
        );
        assert!(slot.is_none());
        assert!(weak.upgrade().is_none());
        reopen_resource(&mut slot, || Ok::<_, ()>(Rc::new(()))).unwrap();
        assert!(slot.is_some());
    }
}

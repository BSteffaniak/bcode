#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_controls_do_not_exhaust_lifetime_or_retarget_stale_senders() {
        let controls = Controls::default();
        for index in 0..5000 {
            let id = format!("search-{index}");
            let (sender, receiver) = mpsc::channel(1);
            controls.0.lock().unwrap().insert(id.clone(), Some(sender));
            let registration = Registration {
                controls: controls.clone(),
                id,
            };
            drop(receiver);
            drop(registration);
        }
        assert!(controls.0.lock().unwrap().is_empty());
        let (old_sender, old_receiver) = mpsc::channel::<CancelReply>(1);
        drop(old_receiver);
        let (current, mut receiver) = mpsc::channel(1);
        controls
            .0
            .lock()
            .unwrap()
            .insert("reused".into(), Some(current));
        let (reply, _) = oneshot::channel();
        assert!(old_sender.try_send(reply).is_err());
        assert!(receiver.try_recv().is_err());
    }
}

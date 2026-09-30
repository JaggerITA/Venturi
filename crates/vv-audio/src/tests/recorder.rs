use super::*;

#[test]
fn a_take_knows_its_length() {
    let take = Take {
        samples: vec![0.0; 48_000 * 2],
        sample_rate: 48_000,
        channels: 2,
    };
    assert_eq!(take.frames(), 48_000);
    assert_eq!(take.seconds(), 1.0);
}

#[test]
#[ignore = "opens the real microphone"]
fn smoke_default_input() {
    eprintln!("devices: {:?}", input_device_names());
    let mut recorder = Recorder::start(None).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    recorder.poll();
    let take = recorder.finish();
    eprintln!(
        "{} Hz, {} ch, {:.2} s",
        take.sample_rate,
        take.channels,
        take.seconds()
    );
    assert!(take.seconds() > 0.1);
}

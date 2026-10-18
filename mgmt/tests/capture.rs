// Capture (boot menu [C]) end to end: `bootrom-mgmt capture` on the client side reads a disk and sends it to the real
// server through the upload API, signed in with the owner account typed on its input.
mod common;

use common::*;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn capture_uploads_the_disk() {
    sandbox("capture_uploads_the_disk", || {
        let home = Home::new("capture");
        let _srv = Server::start(&home);
        let h = admin(&home);

        // 64 MiB GPT disk: p1 8 MiB of data, p2 the rest, zeros but one MiB near its end.
        let disk = home.0.join("pc-disk.img");
        let d = disk.display();
        let st = Command::new("sh")
            .args([
                "-ec",
                &format!(
                    "truncate -s 64M {d}
                     printf 'label: gpt\\nsize=8MiB, type=U\\ntype=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\\n' | sfdisk -q {d}
                     yes broom | head -c 8388608 | dd of={d} bs=1M seek=1 conv=notrunc status=none
                     yes data | head -c 1048576 | dd of={d} bs=1M seek=50 conv=notrunc status=none"
                ),
            ])
            .status()
            .unwrap();
        assert!(st.success());

        let mut c = Command::new(env!("CARGO_BIN_EXE_bootrom-mgmt"))
            .args(["capture", SRV])
            .env("BROOM_CAPTURE_DISK", &disk)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        // Disk 1, the image name, a wrong password first (asked again), then the owner's.
        write!(
            c.stdin.take().unwrap(),
            "1\ncap1\nadmin\nwrong-password\nadmin\n{PASSWORD}\n"
        )
        .unwrap();
        let out = c.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("wrong user or password"), "{text}");
        assert!(text.contains("Image cap1 created"), "{text}");
        assert!(text.contains("All sent"), "{text}");
        // No Windows on this disk: the server's publish refuses it — after the convert.
        assert!(text.contains("could not publish cap1"), "{text}");
        assert_eq!(out.status.code(), Some(1));
        assert!(wait_job(&h, "cap1").starts_with('✗'));

        let img = home.0.join("images/cap1/image.img");
        assert_eq!(
            std::fs::read(&img).unwrap(),
            std::fs::read(&disk).unwrap(),
            "the server's golden is the disk"
        );
        use std::os::unix::fs::MetadataExt;
        assert!(
            std::fs::metadata(&img).unwrap().blocks() * 512 < 20 << 20,
            "zeros stay holes"
        );
    });
}

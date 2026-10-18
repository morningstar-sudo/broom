---
title: Bootrom quán net miễn phí với Broom — boot không ổ cứng Windows 11
description: "Bootrom quán net miễn phí, mã nguồn mở: máy chủ Linux một file, máy trạm Windows 11 boot qua mạng, sạch sau mỗi lần khởi động."
lang: vi
date: 2026-10-11T23:07:00+07:00
last_modified_at: 2026-10-17
faq:
  - q: Broom có miễn phí không?
    a: Có, giấy phép Apache-2.0, dùng cho quán kinh doanh được, không tính phí theo máy.
  - q: Máy trạm có cần ổ cứng không?
    a: Image Windows cần SSD trên mỗi máy (để lưu image và chạy Windows). Image Linux chạy được cả khi không có ổ.
  - q: Bao nhiêu máy?
    a: "Tới khoảng 100 máy mỗi máy chủ, Broom không giới hạn số máy. Cái quyết định là mạng và ổ của máy chủ: tới 30 máy dùng mạng 1 Gbps; 30–60 máy dùng 10 Gbps và ổ NVMe; 60–100 máy dùng 2 cổng 10 Gbps gộp (LACP) hoặc 25 Gbps, NVMe PCIe 4, RAM từ 64 GB. Switch máy trạm 1 Gbps có đường lên 10 Gbps, bật PortFast. Các quán đang chạy hiện đều là phòng nhỏ — tăng máy dần và theo dõi trang Server. Chi tiết: [Sizing](https://github.com/morningstar-sudo/broom#sizing)."
  - q: Có chạy được game có anti-cheat không?
    a: FACEIT Anti-Cheat chạy được với Secure Boot (không cần MOK), TPM 2.0 và HVCI — xem [hướng dẫn FACEIT](faceit-diskless.html).
  - q: Chủ quán có xem được màn hình các máy không?
    a: Có. Trang Remote hiện màn hình mọi máy trạm Windows cùng lúc; bấm vào một máy để điều khiển (chuột, bàn phím, Ctrl+Alt+Del, dán clipboard). Máy trạm không hiện thông báo khi đang bị xem — nếu quy định của quán cần, hãy báo cho khách.
  - q: So với CCBoot thì sao?
    a: Xem [Broom vs CCBoot](broom-vs-ccboot.html).
---

[← Broom](index.md)

**Broom** là phần mềm **bootrom (boot không ổ cứng / diskless)** mã nguồn mở, miễn phí, dành cho **quán net, phòng
game, phòng máy trường học**, tới khoảng 100 máy mỗi máy chủ. Bạn cài Windows 11 Pro **một lần** trong máy ảo, tải
lên máy chủ; mọi máy trạm boot qua mạng từ image đó, và mỗi lần khởi động lại, máy trạm **sạch như mới** — khách cài gì, xoá gì
cũng mất sau khi restart.

![Máy trạm Windows 11 boot qua mạng bằng Broom, có ổ iSCSI G: dùng chung (chứa game)](images/client-games-disk.webp){: width="1280" height="720"}

## Broom có gì

- **Sạch sau mỗi lần khởi động** — mọi thứ khách ghi nằm trên SSD của máy trạm và bị xoá ở lần boot sau.
- **Máy chủ chỉ một file** — chạy trên Linux bất kỳ (đã thử Debian/Ubuntu, Rocky Linux 10), không phải cài DHCP,
  TFTP hay iSCSI riêng: tất cả có sẵn trong Broom.
- **Máy trạm chạy nhanh như ổ cứng thật** — image được lưu trên SSD của từng máy, Windows chạy trực tiếp từ đó; mạng
  chỉ dùng khi có image mới, nên cả phòng boot cùng lúc máy chủ vẫn nhẹ.
- **Ổ iSCSI dùng chung** (chứa game, ứng dụng) — game để trên ổ iSCSI riêng, không nằm trong image. Cập nhật game **trên một máy**, bấm
  Save, cả phòng có bản mới ở lần boot sau; máy đang chơi vẫn dùng bản cũ đến khi restart.
- **Bật Secure Boot không cần MOK** — cả chuỗi boot mạng đã được ký sẵn, không phải enroll khoá trên từng máy.
  Secure Boot, TPM 2.0, HVCI đều bật nên **chạy được FACEIT Anti-Cheat** ([chi tiết](faceit-diskless.md)).
- **Xem và điều khiển máy trạm ngay trên web** — một màn hình chung hiện trực tiếp màn hình của mọi máy; bấm vào một
  máy để dùng chuột, bàn phím như ngồi tại chỗ, kể cả khi khách đang chơi game toàn màn hình. Không cần cài VNC, máy
  trạm không mở cổng nào.
- **Theo dõi sức khoẻ máy** — CPU, RAM, nhiệt độ, tình trạng ổ cứng (SMART, bad sector) của từng máy trạm và của máy
  chủ, báo đỏ khi ổ sắp hỏng hoặc máy quá nóng; máy trạm nào đã bật bao lâu, đang kéo bao nhiêu MB/s từ máy chủ;
  biểu đồ tải máy chủ 10 phút gần nhất và trạng thái từng dịch vụ (DHCP, TFTP, iSCSI).
- **Xem log ngay trên web** — lọc lỗi, tìm theo tên máy, theo dõi trực tiếp lúc máy boot; không cần SSH vào máy chủ.
- **Máy tắt agent là mất mạng** (tuỳ chọn, trang Network → DNS) — máy chủ làm DNS cho cả phòng, chỉ phân giải tên cho
  máy đang chạy agent, máy vừa boot, máy thu ngân / nhân viên được miễn trừ. Khách tắt agent, máy boot không qua mạng,
  laptop lạ cắm vào LAN đều không vào được internet. Nên chặn thêm DNS ra ngoài (cổng 53, 853) trên router.
- **Snapshot / rollback image** tức thì, **chia nhóm máy** (mỗi nhóm một menu boot, image, ổ iSCSI riêng),
  **Wake-on-LAN** và xem trạng thái máy trên web.
- **Miễn phí, mã nguồn mở** (Apache-2.0) — không tính phí theo số máy, dùng cho quán kinh doanh được.

![Trang Remote của Broom: điều khiển một máy trạm Windows 11 ngay trên trình duyệt](images/remote-control.webp){: width="1883" height="867"}

## Cần chuẩn bị

- **Máy chủ:** một máy x86_64 chạy Linux, ổ cứng đủ chứa image, mạng LAN Gigabit tới các máy trạm.
- **Máy trạm:** PC chạy UEFI, đặt boot mạng (PXE) lên đầu, **mỗi máy một SSD** (image Windows được lưu và chạy trên
  đó).
- **Image:** Windows 11 Pro cài trong máy ảo (ví dụ VMware).

## Các bước

### 1. Chạy máy chủ

```bash
sudo mkdir -p /opt/bootrom && cd /opt/bootrom
sudo curl -fLO https://github.com/morningstar-sudo/broom/releases/latest/download/bootrom-mgmt
sudo chmod +x bootrom-mgmt
sudo ./bootrom-mgmt install-service
journalctl -u bootrom-mgmt -f      # hiện mã setup dùng một lần
```

Mở `http://<ip-máy-chủ>/`, nhập mã setup và đặt mật khẩu quản trị.

### 2. Boot mạng

Chọn một trong hai cách:

- **Dùng DHCP của Broom:** tắt DHCP trên router, rồi bật DHCP server ở trang **Network** của Broom; hoặc
- **Giữ DHCP của router:** khai báo next-server / option 66 = IP máy chủ Broom, boot file / option 67 =
  `snponly.efi`.

### 3. Làm image Windows

1. Tạo máy ảo, cài Windows 11 Pro, đến màn hình thiết lập đầu tiên (OOBE) thì bấm **Ctrl+Shift+F3** để vào
   **Audit Mode**.
2. Cài phần mềm, launcher game, cấu hình như muốn cho cả phòng.
3. Trên web Broom, đặt tài khoản và mật khẩu cho khách (**Windows → Guest user**).
4. Trong máy ảo, mở PowerShell bằng quyền Administrator và chạy:

   ```powershell
   irm http://<ip-máy-chủ>/broom-prep-win | iex
   ```

   Máy ảo tự chuẩn bị, sysprep và tắt.

### 4. Tải image lên

Ở trang **Images**, tải cả thư mục máy ảo lên (OS = Windows). Broom tự tách ổ đĩa và publish image.

### 5. Thêm máy trạm và boot

Thêm các máy ở trang **Machines** trước lần boot Windows đầu tiên — máy đã đăng ký và chỉ có một ổ trong thì Broom tự
chia SSD. Bật máy: lần đầu máy tải image về SSD và tự restart một lần; từ đó mỗi lần boot đều sạch.

Máy có **nhiều ổ cứng**: mở **Devices → máy đó → Disks**, bấm **Use as Broom disk** ở ổ muốn chứa Windows (không phải gõ
trên màn hình máy trạm). Các ổ còn lại nên để **ẩn khỏi khách** (chọn cho cả nhóm ở trang **Groups**, hoặc riêng từng
máy): khách không thấy, không ghi được nên không có gì sót lại sau khi restart.

Máy chủ **gắn thêm ổ**: trang **Server → Disks**, bấm **Format** ở ổ mới (gõ tên ổ để xác nhận). Sau đó chọn ổ này khi
tạo image / ổ iSCSI, hoặc chuyển image / ổ iSCSI sang bằng cột **Server disk** ở trang Images / iSCSI Disks.

## Cài game

Ở trang **iSCSI Disks**, tạo ổ iSCSI, chọn một **máy cập nhật**. Boot máy đó, cài hoặc cập nhật game trên ổ iSCSI,
tắt máy đúng cách rồi bấm **Save**. Các máy khác nhận bản mới ở lần boot sau. Chỉ nên để máy cập nhật tải bản cập
nhật game — launcher tự cập nhật trên máy khách sẽ làm đầy SSD và mất khi restart.

## Xem và điều khiển máy trạm

Mở trang **Remote** (nhóm Monitor): màn hình mọi máy trạm Windows hiện thành lưới, cập nhật liên tục. Bấm vào một máy
(hoặc gõ tên máy rồi bấm **Open**) để điều khiển. Bấm vào màn hình trước khi gõ phím. Các nút trên thanh công cụ gửi
**Ctrl+Alt+Del**, phím **Windows**, **Alt+Tab** và **Paste** (dán clipboard của bạn vào máy trạm). Trên Chrome / Edge
hình chạy mượt (H.264); mạng đông thì hạ chất lượng xuống **Low**.

![Trang Remote của Broom: lưới màn hình các máy trạm, lọc theo nhóm, chia trang](images/remote-wall.webp){: width="1907" height="871"}

Sức khoẻ từng máy (CPU, RAM, nhiệt độ, ổ cứng, bad sector, thời gian bật, MB/s đang tải) hiện ở trang **Machines**.
Trang **Server** có tải máy chủ kèm biểu đồ 10 phút, sức khoẻ ổ và bảng **Services** (DHCP, TFTP, iSCSI chạy hay
không). Máy không boot được thì mở **Tools → Logs**, chọn *Warn+* hoặc gõ tên máy để xem lỗi.

![Trang Server của Broom: tải máy chủ và biểu đồ 10 phút gần nhất](images/server-monitoring.webp){: width="1887" height="867"}

## Câu hỏi thường gặp

{% for item in page.faq %}
**{{ item.q }}** {{ item.a | markdownify | remove: '<p>' | remove: '</p>' | strip }}
{% endfor %}

Tài liệu đầy đủ (tiếng Anh): [github.com/morningstar-sudo/broom](https://github.com/morningstar-sudo/broom#readme)

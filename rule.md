# Rule — Quy tắc làm việc

## Sau MỖI khi hoàn thành 1 giai đoạn (bắt buộc)
1. **Review code** phần vừa làm: đúng module, không phình, bám `plan.md`.
2. **Ghi bước test chi tiết** cho giai đoạn hiện tại vào `progress.md` — lệnh cụ thể,
   đầu vào, **kết quả mong đợi vs kết quả thực**. Đủ để người khác chạy lại y hệt.
3. Chỉ đánh dấu phase **done** trong `progress.md` khi test **pass thật** (có bằng
   chứng: log/output/ảnh), không phỏng đoán. Fail → ghi rõ lỗi, **không qua phase sau**.
4. Cập nhật `plan.md` / `rule.md` nếu phát sinh thay đổi thiết kế.

## Nguyên tắc chung
- 1 module = 1 file, không dồn. Tách rõ theo M1–M8.
- Không thêm dependency/lớp trung gian nếu chưa cần (nginx bỏ, NFS thuần cho Linux).
- Golden image chia sẻ **read-only**; ghi runtime: overlay RAM (reset mỗi boot) + data nặng xuống SSD local `/games`.
- Preflight phải pass trước khi app serve (thiếu gói → exit ≠ 0, báo rõ).
- **Scope hiện tại: chỉ Linux diskless (LTSP).** Windows diskless đã gỡ khỏi codebase — tối ưu Linux trước.

## Trạng thái phase
`todo` → `doing` → `done`. Chỉ 1 phase `doing` tại 1 thời điểm.

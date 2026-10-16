// broom-vnc.cs - the Remote agent (vnc.rs). Started by broom-watch.ps1 as SYSTEM in the console session; dials out to
// the server over a WebSocket (the machine opens nothing, so no guest or other LAN host can reach its screen) and,
// while an admin watches, sends JPEG screen updates and plays back the admin's mouse / keys. Compiled to
// broom-vnc.exe by CI (.NET Framework 4, csc, like broom-fw); a local build compiles it once per boot. ASCII only.
//
// Protocol (matches vnc.rs): agent -> server binary "1 x y w h <jpeg>" a changed area, "2 w h" the screen size
// (u16 LE). server/viewer -> agent text JSON: start{q}, stop, ack (the next frame goes only after one is drawn),
// mouse{x,y,b}, wheel{d}, key{code,down}. Positions are over the whole virtual desktop (0..w, 0..h).
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.IO;
using System.Net.WebSockets;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;
using System.Windows.Forms;

public static class BroomVnc {
  // ---- Win32 ----
  [DllImport("user32.dll", SetLastError=true)] static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
  [DllImport("user32.dll", SetLastError=true)] static extern bool SetThreadDesktop(IntPtr desktop);
  [DllImport("user32.dll", SetLastError=true)] static extern bool CloseDesktop(IntPtr desktop);
  [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] static extern int GetSystemMetrics(int index);
  [DllImport("user32.dll")] static extern uint SendInput(uint n, INPUT[] inputs, int size);
  [DllImport("user32.dll")] static extern bool GetCursorInfo(ref CURSORINFO ci);
  [DllImport("user32.dll")] static extern bool DrawIconEx(IntPtr hdc, int x, int y, IntPtr icon, int w, int h, int step, IntPtr brush, int flags);
  [DllImport("gdi32.dll")] static extern bool BitBlt(IntPtr dst, int x, int y, int w, int h, IntPtr src, int sx, int sy, int rop);
  [DllImport("kernel32.dll")] static extern uint WTSGetActiveConsoleSessionId();
  [DllImport("kernel32.dll")] static extern bool ProcessIdToSessionId(uint pid, out uint session);
  [DllImport("kernel32.dll")] static extern uint GetCurrentProcessId();
  [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr h);
  [DllImport("sas.dll", SetLastError=true)] static extern void SendSAS(bool asUser);
  [DllImport("advapi32.dll", SetLastError=true)] static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
  [DllImport("advapi32.dll", SetLastError=true)] static extern bool DuplicateTokenEx(IntPtr token, uint access, IntPtr attr, int level, int type, out IntPtr dup);
  [DllImport("advapi32.dll", SetLastError=true)] static extern bool SetTokenInformation(IntPtr token, int cls, ref uint val, uint len);
  [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern bool CreateProcessAsUser(IntPtr token, string app, string cmd, IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string dir, ref STARTUPINFO si, out PROCESS_INFORMATION pi);
  const int SM_XVIRTUAL=76, SM_YVIRTUAL=77, SM_CXVIRTUAL=78, SM_CYVIRTUAL=79;
  const int SRCCOPY=0x00CC0020, CAPTUREBLT=0x40000000, DI_NORMAL=3;

  [StructLayout(LayoutKind.Sequential)] struct STARTUPINFO { public int cb; public string res1; public string desktop; public string title; public int x, y, xs, ys, xc, yc, fill, flags; public short show, res2; public IntPtr res3, in_, out_, err; }
  [StructLayout(LayoutKind.Sequential)] struct PROCESS_INFORMATION { public IntPtr process, thread; public uint pid, tid; }

  [StructLayout(LayoutKind.Sequential)] struct CURSORINFO { public int cbSize; public int flags; public IntPtr hCursor; public int x; public int y; }
  [StructLayout(LayoutKind.Sequential)] struct INPUT { public uint type; public InputUnion u; }
  [StructLayout(LayoutKind.Explicit)] struct InputUnion { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
  [StructLayout(LayoutKind.Sequential)] struct MOUSEINPUT { public int dx; public int dy; public uint data; public uint flags; public uint time; public IntPtr extra; }
  [StructLayout(LayoutKind.Sequential)] struct KEYBDINPUT { public ushort vk; public ushort scan; public uint flags; public uint time; public IntPtr extra; }

  static int VX, VY, VW, VH;   // capture origin + size (virtual desktop for GDI, the output for Desktop Duplication)
  static volatile int Quality = 60;
  static volatile int MaxW = 0;   // > 0: scale the frame down to this width (the wall's thumbnails)
  static Dupl _dupl;              // Desktop Duplication, re-created after it is lost; null = use GDI
  static int _ddNext;             // TickCount before which not to retry Desktop Duplication
  // The previous frame's 16-row bands, so SendFrame can diff (a reference type: async can't take a ref parameter).
  class FrameState { public byte[][] Prev; }

  static void Main(string[] args) {
    if (args.Length < 1) return;
    // broom-watch starts us as SYSTEM, which lands in session 0 (no desktop to capture or send input to). Relaunch
    // ourselves as SYSTEM inside the console session (where the guest's desktop is), then this copy exits.
    uint console = WTSGetActiveConsoleSessionId();
    uint mine; ProcessIdToSessionId(GetCurrentProcessId(), out mine);
    if (console == 0xFFFFFFFF) return;      // nobody logged on at the console yet: broom-watch retries
    if (mine != console) { RelaunchInSession(console, args[0]); return; }

    // One agent per machine: a second launch (a stale watch task) exits.
    bool fresh;
    using (var mutex = new Mutex(true, "Global\\broom-vnc-agent", out fresh)) {
      if (!fresh) return;
      try { SetProcessDPIAware(); } catch { }
      string url = "ws://" + args[0] + "/api/vnc/agent";
      // Reconnect for ever: the admin may open the viewer at any time, the server restarts, the network blips.
      while (true) {
        try { Run(url).Wait(); } catch { }
        Thread.Sleep(5000);
      }
    }
  }

  // Start broom-vnc.exe again in `session` with our own (SYSTEM) token: same rights, a desktop to work on.
  static void RelaunchInSession(uint session, string srv) {
    IntPtr tok = IntPtr.Zero, dup = IntPtr.Zero;
    try {
      if (!OpenProcessToken(GetCurrentProcess(), 0x0002 | 0x0008, out tok)) return; // DUPLICATE | QUERY
      if (!DuplicateTokenEx(tok, 0x02000000, IntPtr.Zero, 2, 1, out dup)) return;    // MAXIMUM_ALLOWED, primary
      if (!SetTokenInformation(dup, 12, ref session, sizeof(uint))) return;          // TokenSessionId
      var si = new STARTUPINFO(); si.cb = Marshal.SizeOf(typeof(STARTUPINFO)); si.desktop = "winsta0\\default";
      PROCESS_INFORMATION pi;
      string exe = Application.ExecutablePath;
      if (CreateProcessAsUser(dup, exe, "\"" + exe + "\" " + srv, IntPtr.Zero, IntPtr.Zero, false, 0x08000000, IntPtr.Zero, null, ref si, out pi)) {
        CloseHandle(pi.process); CloseHandle(pi.thread);
      }
    } finally {
      if (dup != IntPtr.Zero) CloseHandle(dup);
      if (tok != IntPtr.Zero) CloseHandle(tok);
    }
  }

  static async Task Run(string url) {
    using (var ws = new ClientWebSocket()) {
      var cts = new CancellationTokenSource();
      await ws.ConnectAsync(new Uri(url), cts.Token);
      bool streaming = false;
      var frameReady = new SemaphoreSlim(1); // one in flight: refilled by the viewer's "ack"
      var sendLock = new SemaphoreSlim(1);
      var fr = new FrameState();
      var capture = Task.Run(async () => {
        while (ws.State == WebSocketState.Open) {
          if (!streaming) { await Task.Delay(100); continue; }
          if (!await frameReady.WaitAsync(1000)) continue;
          bool sent = false;
          try { sent = await SendFrame(ws, sendLock, fr); } catch { }
          if (!sent) { frameReady.Release(); await Task.Delay(50); } // nothing changed: free the slot, look again soon
        }
      });
      var buf = new byte[8192];
      var msg = new List<byte>();
      while (ws.State == WebSocketState.Open) {
        var r = await ws.ReceiveAsync(new ArraySegment<byte>(buf), cts.Token);
        if (r.MessageType == WebSocketMessageType.Close) break;
        msg.AddRange(new ArraySegment<byte>(buf, 0, r.Count));
        if (!r.EndOfMessage) continue;
        string text = System.Text.Encoding.UTF8.GetString(msg.ToArray());
        msg.Clear();
        string op = Field(text, "op");
        if (op == "start") { Quality = Clamp(IntField(text, "q", 60), 20, 95); MaxW = IntField(text, "w", 0); fr.Prev = null; streaming = true; Drain(frameReady); frameReady.Release(); }
        else if (op == "stop") { streaming = false; }
        else if (op == "ack") { if (frameReady.CurrentCount == 0) frameReady.Release(); }
        else if (op == "mouse") { Mouse(IntField(text, "x", 0), IntField(text, "y", 0), IntField(text, "b", 0)); }
        else if (op == "wheel") { Wheel(IntField(text, "d", 0)); }
        else if (op == "key") { Key(Field(text, "code"), text.Contains("\"down\":true")); }
        else if (op == "sas") { try { SendSAS(false); } catch { } } // Ctrl+Alt+Del (we are SYSTEM; policy set in broom-done)
        else if (op == "clip") { SetClip(ReadString(text, "text")); }
      }
      streaming = false;
      cts.Cancel();
      try { await capture; } catch { }
    }
  }

  static void Drain(SemaphoreSlim s) { while (s.CurrentCount > 0) s.Wait(0); }

  // Put text on the machine's clipboard (the admin's "paste"). Clipboard access must be on an STA thread, in the
  // console session this agent already runs in.
  static void SetClip(string text) {
    var t = new Thread(() => {
      try { if (string.IsNullOrEmpty(text)) Clipboard.Clear(); else Clipboard.SetText(text); } catch { }
    });
    t.SetApartmentState(ApartmentState.STA);
    t.IsBackground = true;
    t.Start();
    t.Join(2000);
  }

  // One screen grab, cursor drawn in, VX/VY/VW/VH set. null = no new frame (skip). Desktop Duplication is tried first
  // (it captures exclusive-fullscreen games, which GDI shows black); GDI covers what it can't (the secure desktop /
  // UAC prompt, and the moment a game switches modes). Scaled down to MaxW for the wall's thumbnails.
  static Bitmap Capture() {
    int cap; Bitmap raw = DuplCapture(out cap);
    if (cap == 1) return null;              // Desktop Duplication: no change since the last frame
    if (raw == null) raw = GdiCapture();    // unavailable / lost this frame -> GDI
    if (raw == null) return null;
    int mw = MaxW;
    if (mw > 0 && raw.Width > mw) {
      int w = mw, h = Math.Max(1, (int)((long)raw.Height * mw / raw.Width));
      var small = new Bitmap(w, h, PixelFormat.Format32bppRgb);
      using (var g = Graphics.FromImage(small)) {
        g.InterpolationMode = InterpolationMode.Low;
        g.DrawImage(raw, 0, 0, w, h);
      }
      raw.Dispose(); VW = w; VH = h; return small;
    }
    return raw;
  }

  // Desktop Duplication (DXGI). cap: 0 got a frame, 1 no change, 2 unavailable (use GDI). Lost (fullscreen / desktop
  // switch) -> dropped and retried after a short wait, GDI covering meanwhile.
  static Bitmap DuplCapture(out int cap) {
    cap = 2;
    int now = Environment.TickCount;
    if (_dupl == null) {
      if (now - _ddNext < 0) return null;
      try { var d = new Dupl(); if (d.Init()) _dupl = d; else { d.Dispose(); _ddNext = now + 1500; return null; } }
      catch { _ddNext = now + 1500; return null; }
    }
    int r; Bitmap bmp = _dupl.Grab(out r);
    if (r == 0 && bmp != null) {
      VX = _dupl.Left; VY = _dupl.Top; VW = _dupl.W; VH = _dupl.H;
      using (var g = Graphics.FromImage(bmp)) { IntPtr hdc = g.GetHdc(); DrawCursor(hdc); g.ReleaseHdc(hdc); }
      cap = 0; return bmp;
    }
    if (r == 1) { cap = 1; return null; }   // timeout: nothing changed
    _dupl.Dispose(); _dupl = null; _ddNext = now + 800;  // lost: GDI this frame, retry soon
    return null;
  }

  // GDI capture of the whole virtual desktop. The input desktop is grabbed each time so a UAC / lock screen follows.
  static Bitmap GdiCapture() {
    VX = GetSystemMetrics(SM_XVIRTUAL); VY = GetSystemMetrics(SM_YVIRTUAL);
    VW = GetSystemMetrics(SM_CXVIRTUAL); VH = GetSystemMetrics(SM_CYVIRTUAL);
    if (VW <= 0 || VH <= 0) return null;
    IntPtr desk = OpenInputDesktop(0, false, 0x0100 | 0x0001); // DESKTOP_SWITCHDESKTOP | DESKTOP_READOBJECTS
    if (desk != IntPtr.Zero) SetThreadDesktop(desk);
    try {
      var bmp = new Bitmap(VW, VH, PixelFormat.Format32bppRgb);
      using (var g = Graphics.FromImage(bmp)) {
        IntPtr hdc = g.GetHdc();
        bool ok;
        using (var screen = Graphics.FromHwnd(IntPtr.Zero)) {
          IntPtr src = screen.GetHdc();
          ok = BitBlt(hdc, 0, 0, VW, VH, src, VX, VY, SRCCOPY | CAPTUREBLT);
          screen.ReleaseHdc(src);
        }
        DrawCursor(hdc); g.ReleaseHdc(hdc);
        if (!ok) { bmp.Dispose(); return null; }
      }
      return bmp;
    } finally {
      if (desk != IntPtr.Zero) CloseDesktop(desk);
    }
  }

  static async Task<bool> SendFrame(ClientWebSocket ws, SemaphoreSlim sendLock, FrameState fr) {
    byte[][] prev = fr.Prev;
    using (var bmp = Capture()) {
      if (bmp == null) return false;
      int w = bmp.Width, h = bmp.Height;
      // Changed-strip detection: 16-row bands compared to the previous frame -> the bounding rectangle of the bands
      // that differ. First frame (prev == null) or a size change -> whole screen + the size header.
      bool first = prev == null || prev.Length != (h + 15) / 16;
      var data = bmp.LockBits(new Rectangle(0, 0, w, h), ImageLockMode.ReadOnly, PixelFormat.Format32bppRgb);
      int bands = (h + 15) / 16, stride = data.Stride;
      var cur = new byte[bands][];
      int top = -1, bottom = -1;
      for (int b = 0; b < bands; b++) {
        int y0 = b * 16, rows = Math.Min(16, h - y0);
        var band = new byte[rows * stride];
        Marshal.Copy(data.Scan0 + y0 * stride, band, 0, band.Length);
        cur[b] = band;
        if (first || !Same(band, prev[b])) { if (top < 0) top = y0; bottom = y0 + rows; }
      }
      bmp.UnlockBits(data);
      fr.Prev = cur;
      if (top < 0) return false; // nothing changed

      if (first) await Send(ws, sendLock, Header2(w, h));
      using (var sub = bmp.Clone(new Rectangle(0, top, w, bottom - top), PixelFormat.Format24bppRgb))
      using (var ms = new MemoryStream()) {
        sub.Save(ms, Jpeg(), JpegParams(Quality));
        await Send(ws, sendLock, FrameMsg(0, top, w, bottom - top, ms.ToArray()));
      }
      return true;
    }
  }

  static bool Same(byte[] a, byte[] b) {
    if (b == null || a.Length != b.Length) return false;
    for (int i = 0; i < a.Length; i++) if (a[i] != b[i]) return false;
    return true;
  }

  static void DrawCursor(IntPtr hdc) {
    var ci = new CURSORINFO(); ci.cbSize = Marshal.SizeOf(ci);
    if (GetCursorInfo(ref ci) && ci.flags == 1 && ci.hCursor != IntPtr.Zero)
      DrawIconEx(hdc, ci.x - VX - 16, ci.y - VY - 16, ci.hCursor, 0, 0, 0, IntPtr.Zero, DI_NORMAL);
  }

  static byte[] Header2(int w, int h) {
    var b = new byte[5]; b[0] = 2;
    b[1] = (byte)w; b[2] = (byte)(w >> 8); b[3] = (byte)h; b[4] = (byte)(h >> 8);
    return b;
  }
  static byte[] FrameMsg(int x, int y, int w, int h, byte[] jpeg) {
    var b = new byte[9 + jpeg.Length]; b[0] = 1;
    U16(b, 1, x); U16(b, 3, y); U16(b, 5, w); U16(b, 7, h);
    Buffer.BlockCopy(jpeg, 0, b, 9, jpeg.Length);
    return b;
  }
  static void U16(byte[] b, int at, int v) { b[at] = (byte)v; b[at + 1] = (byte)(v >> 8); }

  static async Task Send(ClientWebSocket ws, SemaphoreSlim sendLock, byte[] data) {
    await sendLock.WaitAsync();
    try { await ws.SendAsync(new ArraySegment<byte>(data), WebSocketMessageType.Binary, true, CancellationToken.None); }
    finally { sendLock.Release(); }
  }

  static ImageCodecInfo _jpeg;
  static ImageCodecInfo Jpeg() {
    if (_jpeg == null) foreach (var c in ImageCodecInfo.GetImageEncoders()) if (c.FormatID == ImageFormat.Jpeg.Guid) _jpeg = c;
    return _jpeg;
  }
  static EncoderParameters JpegParams(int q) {
    var p = new EncoderParameters(1);
    p.Param[0] = new EncoderParameter(Encoder.Quality, (long)q);
    return p;
  }

  // ---- input ----
  static void Mouse(int x, int y, int buttons) {
    // Absolute mouse is 0..65535 over the whole virtual desktop.
    int ax = VW > 1 ? (int)((long)x * 65535 / (VW - 1)) : 0;
    int ay = VH > 1 ? (int)((long)y * 65535 / (VH - 1)) : 0;
    uint flags = 0x8000 | 0x4000 | 0x0001; // ABSOLUTE | VIRTUALDESK | MOVE
    flags |= DownUp(buttons, 1, 0x0002, 0x0004); // left
    flags |= DownUp(buttons, 2, 0x0008, 0x0010); // right
    flags |= DownUp(buttons, 4, 0x0020, 0x0040); // middle
    _lastButtons = buttons;
    SendMouse(ax, ay, flags, 0);
  }
  static int _lastButtons = 0;
  static uint DownUp(int now, int bit, uint down, uint up) {
    bool a = (now & bit) != 0, b = (_lastButtons & bit) != 0;
    if (a && !b) return down;
    if (!a && b) return up;
    return 0;
  }
  static void Wheel(int d) { SendMouse(0, 0, 0x0800, (uint)(d * 120)); } // WHEEL, no move
  static void SendMouse(int ax, int ay, uint flags, uint data) {
    var inp = new INPUT[1]; inp[0].type = 0; // MOUSE
    inp[0].u.mi = new MOUSEINPUT { dx = ax, dy = ay, data = data, flags = flags };
    SendInput(1, inp, Marshal.SizeOf(typeof(INPUT)));
  }
  static void Key(string code, bool down) {
    int sc = Scancode(code);
    if (sc == 0) return;
    bool ext = (sc & 0xE000) == 0xE000;
    uint flags = 0x0008; // SCANCODE
    if (ext) flags |= 0x0001; // EXTENDEDKEY
    if (!down) flags |= 0x0002; // KEYUP
    var inp = new INPUT[1]; inp[0].type = 1; // KEYBOARD
    inp[0].u.ki = new KEYBDINPUT { vk = 0, scan = (ushort)(sc & 0xFF), flags = flags };
    SendInput(1, inp, Marshal.SizeOf(typeof(INPUT)));
  }


  // ---- tiny JSON field readers (the server sends flat objects) ----
  static string Field(string json, string key) {
    string k = "\"" + key + "\"";
    int i = json.IndexOf(k); if (i < 0) return "";
    i = json.IndexOf(':', i + k.Length); if (i < 0) return "";
    i++; while (i < json.Length && (json[i] == ' ')) i++;
    if (i >= json.Length) return "";
    if (json[i] == '"') { int e = json.IndexOf('"', i + 1); return e < 0 ? "" : json.Substring(i + 1, e - i - 1); }
    int j = i; while (j < json.Length && json[j] != ',' && json[j] != '}') j++;
    return json.Substring(i, j - i).Trim();
  }
  static int IntField(string json, string key, int dflt) { int v; return int.TryParse(Field(json, key), out v) ? v : dflt; }

  // A string value parsed with its JSON escapes (clipboard text has quotes, newlines, \uXXXX). "" if absent.
  static string ReadString(string json, string key) {
    string k = "\"" + key + "\"";
    int i = json.IndexOf(k); if (i < 0) return "";
    i = json.IndexOf(':', i + k.Length); if (i < 0) return "";
    i++; while (i < json.Length && json[i] == ' ') i++;
    if (i >= json.Length || json[i] != '"') return "";
    i++;
    var sb = new System.Text.StringBuilder();
    while (i < json.Length) {
      char c = json[i++];
      if (c == '"') break;
      if (c != '\\') { sb.Append(c); continue; }
      if (i >= json.Length) break;
      char e = json[i++];
      switch (e) {
        case 'n': sb.Append('\n'); break;
        case 'r': sb.Append('\r'); break;
        case 't': sb.Append('\t'); break;
        case 'b': sb.Append('\b'); break;
        case 'f': sb.Append('\f'); break;
        case 'u':
          if (i + 4 <= json.Length) {
            int code;
            if (int.TryParse(json.Substring(i, 4), System.Globalization.NumberStyles.HexNumber, null, out code))
              sb.Append((char)code);
            i += 4;
          }
          break;
        default: sb.Append(e); break; // \" \\ \/
      }
    }
    return sb.ToString();
  }
  static int Clamp(int v, int lo, int hi) { return v < lo ? lo : (v > hi ? hi : v); }

  // KeyboardEvent.code -> PS/2 set 1 scancode (0xE000 marks an extended key). Enough for a keyboard; unknown -> 0.
  static int Scancode(string c) {
    switch (c) {
      case "Escape": return 0x01;
      case "Digit1": return 0x02; case "Digit2": return 0x03; case "Digit3": return 0x04; case "Digit4": return 0x05;
      case "Digit5": return 0x06; case "Digit6": return 0x07; case "Digit7": return 0x08; case "Digit8": return 0x09;
      case "Digit9": return 0x0A; case "Digit0": return 0x0B; case "Minus": return 0x0C; case "Equal": return 0x0D;
      case "Backspace": return 0x0E; case "Tab": return 0x0F;
      case "KeyQ": return 0x10; case "KeyW": return 0x11; case "KeyE": return 0x12; case "KeyR": return 0x13;
      case "KeyT": return 0x14; case "KeyY": return 0x15; case "KeyU": return 0x16; case "KeyI": return 0x17;
      case "KeyO": return 0x18; case "KeyP": return 0x19; case "BracketLeft": return 0x1A; case "BracketRight": return 0x1B;
      case "Enter": return 0x1C; case "ControlLeft": return 0x1D;
      case "KeyA": return 0x1E; case "KeyS": return 0x1F; case "KeyD": return 0x20; case "KeyF": return 0x21;
      case "KeyG": return 0x22; case "KeyH": return 0x23; case "KeyJ": return 0x24; case "KeyK": return 0x25;
      case "KeyL": return 0x26; case "Semicolon": return 0x27; case "Quote": return 0x28; case "Backquote": return 0x29;
      case "ShiftLeft": return 0x2A; case "Backslash": return 0x2B;
      case "KeyZ": return 0x2C; case "KeyX": return 0x2D; case "KeyC": return 0x2E; case "KeyV": return 0x2F;
      case "KeyB": return 0x30; case "KeyN": return 0x31; case "KeyM": return 0x32; case "Comma": return 0x33;
      case "Period": return 0x34; case "Slash": return 0x35; case "ShiftRight": return 0x36;
      case "NumpadMultiply": return 0x37; case "AltLeft": return 0x38; case "Space": return 0x39; case "CapsLock": return 0x3A;
      case "F1": return 0x3B; case "F2": return 0x3C; case "F3": return 0x3D; case "F4": return 0x3E; case "F5": return 0x3F;
      case "F6": return 0x40; case "F7": return 0x41; case "F8": return 0x42; case "F9": return 0x43; case "F10": return 0x44;
      case "NumLock": return 0x45; case "ScrollLock": return 0x46;
      case "Numpad7": return 0x47; case "Numpad8": return 0x48; case "Numpad9": return 0x49; case "NumpadSubtract": return 0x4A;
      case "Numpad4": return 0x4B; case "Numpad5": return 0x4C; case "Numpad6": return 0x4D; case "NumpadAdd": return 0x4E;
      case "Numpad1": return 0x4F; case "Numpad2": return 0x50; case "Numpad3": return 0x51; case "Numpad0": return 0x52;
      case "NumpadDecimal": return 0x53; case "F11": return 0x57; case "F12": return 0x58;
      // extended keys (0xE0 prefix)
      case "NumpadEnter": return 0xE01C; case "ControlRight": return 0xE01D; case "NumpadDivide": return 0xE035;
      case "AltRight": return 0xE038; case "Home": return 0xE047; case "ArrowUp": return 0xE048; case "PageUp": return 0xE049;
      case "ArrowLeft": return 0xE04B; case "ArrowRight": return 0xE04D; case "End": return 0xE04F; case "ArrowDown": return 0xE050;
      case "PageDown": return 0xE051; case "Insert": return 0xE052; case "Delete": return 0xE053;
      case "MetaLeft": return 0xE05B; case "MetaRight": return 0xE05C; case "ContextMenu": return 0xE05D;
      default: return 0;
    }
  }

  // ---- Desktop Duplication (DXGI) ----
  // The composed desktop, including exclusive-fullscreen games that GDI shows black. Raw COM interop (no SharpDX: the
  // client compiles this one file with csc, no packages), so interfaces are called by their vtable index. Captures the
  // primary output; a UAC / lock screen makes it lose access -> the caller falls back to GDI.
  [DllImport("d3d11.dll")] static extern int D3D11CreateDevice(IntPtr adapter, int driverType, IntPtr software, uint flags, IntPtr levels, uint numLevels, uint sdk, out IntPtr device, out int level, out IntPtr ctx);
  [DllImport("kernel32.dll", EntryPoint="RtlMoveMemory")] static extern void CopyMemory(IntPtr dst, IntPtr src, int n);

  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int QiFn(IntPtr self, ref Guid iid, out IntPtr ppv);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int RelFn(IntPtr self);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int GetAdapterFn(IntPtr self, out IntPtr a);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int EnumOutputsFn(IntPtr self, uint i, out IntPtr o);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int OutDescFn(IntPtr self, out DXGI_OUTPUT_DESC d);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int DuplicateFn(IntPtr self, IntPtr dev, out IntPtr dupl);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int AcquireFn(IntPtr self, uint ms, out DXGI_OUTDUPL_FRAME_INFO info, out IntPtr res);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int CreateTexFn(IntPtr self, ref D3D11_TEXTURE2D_DESC d, IntPtr init, out IntPtr tex);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate void CopyResFn(IntPtr self, IntPtr dst, IntPtr src);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int MapFn(IntPtr self, IntPtr res, uint sub, uint type, uint flags, out D3D11_MAPPED_SUBRESOURCE m);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate void UnmapFn(IntPtr self, IntPtr res, uint sub);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate void TexDescFn(IntPtr self, out D3D11_TEXTURE2D_DESC d);

  [StructLayout(LayoutKind.Sequential)] struct RECTi { public int left, top, right, bottom; }
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct DXGI_OUTPUT_DESC { [MarshalAs(UnmanagedType.ByValTStr, SizeConst=32)] public string DeviceName; public RECTi DesktopCoordinates; public int AttachedToDesktop; public int Rotation; public IntPtr Monitor; }
  [StructLayout(LayoutKind.Sequential)] struct POINTi { public int x, y; }
  [StructLayout(LayoutKind.Sequential)] struct PTR_POS { public POINTi Position; public int Visible; }
  [StructLayout(LayoutKind.Sequential)] struct DXGI_OUTDUPL_FRAME_INFO { public long LastPresentTime, LastMouseUpdateTime; public uint AccumulatedFrames; public int RectsCoalesced, ProtectedContentMaskedOut; public PTR_POS PointerPosition; public uint TotalMetadataBufferSize, PointerShapeBufferSize; }
  [StructLayout(LayoutKind.Sequential)] struct SAMPLE_DESC { public uint Count, Quality; }
  [StructLayout(LayoutKind.Sequential)] struct D3D11_TEXTURE2D_DESC { public uint Width, Height, MipLevels, ArraySize, Format; public SAMPLE_DESC SampleDesc; public uint Usage, BindFlags, CPUAccessFlags, MiscFlags; }
  [StructLayout(LayoutKind.Sequential)] struct D3D11_MAPPED_SUBRESOURCE { public IntPtr pData; public uint RowPitch, DepthPitch; }

  static Guid IID_IDXGIDevice = new Guid("54ec77fa-1377-44e6-8c32-88fd5f44c84c");
  static Guid IID_IDXGIOutput1 = new Guid("00cddea8-939b-4b83-a340-a685226666cc");
  static Guid IID_ID3D11Texture2D = new Guid("6f15aaf2-d208-4e89-9ab4-489535d34f9c");
  const int DXGI_WAIT_TIMEOUT = unchecked((int)0x887A0027);

  static T Vt<T>(IntPtr obj, int i) {
    IntPtr vtbl = Marshal.ReadIntPtr(obj);
    return (T)(object)Marshal.GetDelegateForFunctionPointer(Marshal.ReadIntPtr(vtbl, i * IntPtr.Size), typeof(T));
  }
  static void Rel(IntPtr p) { if (p != IntPtr.Zero) Vt<RelFn>(p, 2)(p); }

  class Dupl : IDisposable {
    IntPtr device, ctx, dupl, staging;
    int sw, sh; uint sfmt;
    public int W, H, Left, Top;

    public bool Init() {
      int lvl;
      if (D3D11CreateDevice(IntPtr.Zero, 1, IntPtr.Zero, 0, IntPtr.Zero, 0, 7, out device, out lvl, out ctx) != 0) return false; // HARDWARE, SDK 7
      IntPtr dxgi; Guid gd = IID_IDXGIDevice;
      if (Vt<QiFn>(device, 0)(device, ref gd, out dxgi) != 0) return false;
      IntPtr adapter; int hr = Vt<GetAdapterFn>(dxgi, 7)(dxgi, out adapter); Rel(dxgi); if (hr != 0) return false;
      IntPtr output; hr = Vt<EnumOutputsFn>(adapter, 7)(adapter, 0, out output); Rel(adapter); if (hr != 0) return false;
      DXGI_OUTPUT_DESC od; Vt<OutDescFn>(output, 7)(output, out od);
      Left = od.DesktopCoordinates.left; Top = od.DesktopCoordinates.top;
      IntPtr out1; Guid g1 = IID_IDXGIOutput1; hr = Vt<QiFn>(output, 0)(output, ref g1, out out1); Rel(output); if (hr != 0) return false;
      hr = Vt<DuplicateFn>(out1, 22)(out1, device, out dupl); Rel(out1);
      return hr == 0 && dupl != IntPtr.Zero;
    }

    void EnsureStaging(int w, int h, uint fmt) {
      if (staging != IntPtr.Zero && sw == w && sh == h && sfmt == fmt) return;
      if (staging != IntPtr.Zero) { Rel(staging); staging = IntPtr.Zero; }
      var d = new D3D11_TEXTURE2D_DESC { Width = (uint)w, Height = (uint)h, MipLevels = 1, ArraySize = 1, Format = fmt,
        Usage = 3, BindFlags = 0, CPUAccessFlags = 0x20000, MiscFlags = 0 }; // STAGING, CPU_ACCESS_READ
      d.SampleDesc.Count = 1;
      Vt<CreateTexFn>(device, 5)(device, ref d, IntPtr.Zero, out staging);
      sw = w; sh = h; sfmt = fmt;
    }

    // r: 0 got a frame, 1 timeout (no change), 2 lost / error.
    public Bitmap Grab(out int r) {
      r = 2;
      DXGI_OUTDUPL_FRAME_INFO info; IntPtr res;
      int hr = Vt<AcquireFn>(dupl, 8)(dupl, 100, out info, out res);
      if (hr == DXGI_WAIT_TIMEOUT) { r = 1; return null; }
      if (hr != 0) return null;
      Bitmap bmp = null;
      try {
        IntPtr tex; Guid gt = IID_ID3D11Texture2D;
        if (Vt<QiFn>(res, 0)(res, ref gt, out tex) != 0) return null;
        try {
          D3D11_TEXTURE2D_DESC desc; Vt<TexDescFn>(tex, 10)(tex, out desc);
          W = (int)desc.Width; H = (int)desc.Height;
          EnsureStaging(W, H, desc.Format);
          if (staging == IntPtr.Zero) return null;
          Vt<CopyResFn>(ctx, 47)(ctx, staging, tex);
          D3D11_MAPPED_SUBRESOURCE map;
          if (Vt<MapFn>(ctx, 14)(ctx, staging, 0, 1, 0, out map) != 0) return null; // MAP_READ
          try {
            bmp = new Bitmap(W, H, PixelFormat.Format32bppRgb);
            var bd = bmp.LockBits(new Rectangle(0, 0, W, H), ImageLockMode.WriteOnly, PixelFormat.Format32bppRgb);
            int rowBytes = W * 4;
            for (int y = 0; y < H; y++)
              CopyMemory(IntPtr.Add(bd.Scan0, y * bd.Stride), IntPtr.Add(map.pData, y * (int)map.RowPitch), rowBytes);
            bmp.UnlockBits(bd);
          } finally { Vt<UnmapFn>(ctx, 15)(ctx, staging, 0); }
        } finally { Rel(tex); }
      } finally {
        Vt<RelFn>(dupl, 14)(dupl); // ReleaseFrame
        Rel(res);
      }
      r = 0; return bmp;
    }

    public void Dispose() {
      Rel(staging); Rel(dupl); Rel(ctx); Rel(device);
      staging = dupl = ctx = device = IntPtr.Zero;
    }
  }
}

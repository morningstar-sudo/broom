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
using System.Management;
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
  [DllImport("user32.dll")] static extern bool GetIconInfo(IntPtr icon, out ICONINFO info);
  [DllImport("gdi32.dll")] static extern bool DeleteObject(IntPtr obj);
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

  // CharSet.Unicode: CreateProcessAsUser here is the W function, so lpDesktop must be UTF-16; marshaled as ANSI it
  // names no desktop and the child dies while user32 loads (0xC0000142), before any of our code runs.
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct STARTUPINFO { public int cb; public string res1; public string desktop; public string title; public int x, y, xs, ys, xc, yc, fill, flags; public short show, res2; public IntPtr res3, in_, out_, err; }
  [StructLayout(LayoutKind.Sequential)] struct PROCESS_INFORMATION { public IntPtr process, thread; public uint pid, tid; }

  [StructLayout(LayoutKind.Sequential)] struct CURSORINFO { public int cbSize; public int flags; public IntPtr hCursor; public int x; public int y; }
  [StructLayout(LayoutKind.Sequential)] struct ICONINFO { public bool fIcon; public int xHotspot; public int yHotspot; public IntPtr hbmMask; public IntPtr hbmColor; }
  [StructLayout(LayoutKind.Sequential)] struct INPUT { public uint type; public InputUnion u; }
  [StructLayout(LayoutKind.Explicit)] struct InputUnion { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
  [StructLayout(LayoutKind.Sequential)] struct MOUSEINPUT { public int dx; public int dy; public uint data; public uint flags; public uint time; public IntPtr extra; }
  [StructLayout(LayoutKind.Sequential)] struct KEYBDINPUT { public ushort vk; public ushort scan; public uint flags; public uint time; public IntPtr extra; }

  static int VX, VY, VW, VH;   // capture origin + size (virtual desktop for GDI, the output for Desktop Duplication)
  static volatile int Quality = 60;
  static volatile int MaxW = 0;   // > 0: scale the frame down to this width (the wall's thumbnails)
  static volatile int WantH264 = 0; // the viewer asked for H.264
  static int Codec = 0;           // what we are sending: 0 JPEG, 1 H.264 (decided on the first frame after start)
  static H264 _h264;              // the encoder while Codec == 1
  static Dupl _dupl;              // Desktop Duplication, re-created after it is lost; null = use GDI
  static int _ddNext;             // TickCount before which not to retry Desktop Duplication
  // The previous frame's 16-row bands, so SendFrame can diff (a reference type: async can't take a ref parameter).
  // Cur: the cursor shape the viewer was last sent (-1 = none yet, Zero = hidden).
  class FrameState { public byte[][] Prev; public IntPtr Cur = new IntPtr(-1); }

  static void Main(string[] args) {
    if (args.Length < 1) return;
    // broom-watch starts us as SYSTEM, which lands in session 0 (no desktop to capture or send input to). Relaunch
    // ourselves as SYSTEM inside the console session (where the guest's desktop is), then this copy exits.
    // Anything that escapes (a crash before the first log line would otherwise leave no trace) goes to the log.
    AppDomain.CurrentDomain.UnhandledException += (s, e) => Log("crash: " + e.ExceptionObject);
    uint console = WTSGetActiveConsoleSessionId();
    uint mine; ProcessIdToSessionId(GetCurrentProcessId(), out mine);
    if (mine == console) Log("start in session " + mine);
    if (console == 0xFFFFFFFF) { Log("no console session yet"); return; } // broom-watch retries
    if (mine != console) {
      // broom-watch calls us every 15 s: an agent already running (its mutex exists) -> nothing to start.
      Mutex running;
      if (Mutex.TryOpenExisting("Global\\broom-vnc-agent", out running)) { running.Dispose(); return; }
      RelaunchInSession(console, args[0]);
      return;
    }

    // One agent per machine: a second launch (a stale watch task) exits.
    bool fresh;
    using (var mutex = new Mutex(true, "Global\\broom-vnc-agent", out fresh)) {
      if (!fresh) { Log("another agent already runs - exiting"); return; }
      Log("agent started in session " + mine + ", server " + args[0]);
      try { SetProcessDPIAware(); } catch { }
      string url = "ws://" + args[0] + "/api/vnc/agent";
      // Reconnect for ever: the admin may open the viewer at any time, the server restarts, the network blips.
      string last = null;
      while (true) {
        string err = null;
        try { Run(url).Wait(); err = "connection closed"; }
        catch (AggregateException e) { err = e.InnerException != null ? e.InnerException.Message : e.Message; }
        catch (Exception e) { err = e.Message; }
        if (err != last) { Log("server connection: " + err + " (retrying every 5 s)"); last = err; }
        Thread.Sleep(5000);
      }
    }
  }

  // C:\Windows\Temp\broom-vnc.log: what the agent did (start, relaunch, connection errors), each change once. Kept
  // small: cut when it passes 256 KB. Never throws.
  static void Log(string m) {
    try {
      string f = Environment.GetEnvironmentVariable("SystemRoot") + "\\Temp\\broom-vnc.log";
      if (File.Exists(f) && new FileInfo(f).Length > 256 * 1024) File.Delete(f);
      File.AppendAllText(f, DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss") + " [" + GetCurrentProcessId() + "] " + m + "\r\n");
    } catch { }
  }

  [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern bool LookupPrivilegeValueW(string system, string name, out long luid);
  [DllImport("advapi32.dll", SetLastError=true)] static extern bool AdjustTokenPrivileges(IntPtr token, bool disableAll, ref TokenPriv tp, uint len, IntPtr prev, IntPtr retLen);
  [StructLayout(LayoutKind.Sequential, Pack = 4)] struct TokenPriv { public uint Count; public long Luid; public uint Attr; }

  // SYSTEM holds these, but not always enabled: SetTokenInformation(TokenSessionId) needs SeTcb, CreateProcessAsUser
  // SeAssignPrimaryToken + SeIncreaseQuota.
  static void EnablePrivileges() {
    IntPtr t;
    if (!OpenProcessToken(GetCurrentProcess(), 0x28, out t)) return; // TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY
    try {
      foreach (var name in new[] { "SeTcbPrivilege", "SeAssignPrimaryTokenPrivilege", "SeIncreaseQuotaPrivilege" }) {
        var tp = new TokenPriv { Count = 1, Attr = 2 }; // SE_PRIVILEGE_ENABLED
        if (LookupPrivilegeValueW(null, name, out tp.Luid)) AdjustTokenPrivileges(t, false, ref tp, 0, IntPtr.Zero, IntPtr.Zero);
      }
    } finally { CloseHandle(t); }
  }

  // Start broom-vnc.exe again in `session` with our own (SYSTEM) token: same rights, a desktop to work on.
  static void RelaunchInSession(uint session, string srv) {
    IntPtr tok = IntPtr.Zero, dup = IntPtr.Zero;
    try {
      EnablePrivileges();
      if (!OpenProcessToken(GetCurrentProcess(), 0x0002 | 0x0008, out tok)) { Log("relaunch: OpenProcessToken error " + Marshal.GetLastWin32Error()); return; }
      if (!DuplicateTokenEx(tok, 0x02000000, IntPtr.Zero, 2, 1, out dup)) { Log("relaunch: DuplicateTokenEx error " + Marshal.GetLastWin32Error()); return; }
      if (!SetTokenInformation(dup, 12, ref session, sizeof(uint))) { Log("relaunch: SetTokenInformation(session " + session + ") error " + Marshal.GetLastWin32Error()); return; }
      var si = new STARTUPINFO(); si.cb = Marshal.SizeOf(typeof(STARTUPINFO)); si.desktop = "winsta0\\default";
      PROCESS_INFORMATION pi;
      string exe = Application.ExecutablePath;
      // broom-watch is a Task Scheduler task: we sit in its job object, and a child in another session left in that job
      // can be ended at once. Break away from it (CREATE_BREAKAWAY_FROM_JOB); a job that forbids it -> as before.
      const uint NO_WINDOW = 0x08000000, BREAKAWAY = 0x01000000;
      bool ok = CreateProcessAsUser(dup, exe, "\"" + exe + "\" " + srv, IntPtr.Zero, IntPtr.Zero, false, NO_WINDOW | BREAKAWAY, IntPtr.Zero, null, ref si, out pi);
      string how = "out of the task's job";
      if (!ok) {
        int e = Marshal.GetLastWin32Error();
        how = "inside the task's job (breakaway refused: error " + e + ")";
        ok = CreateProcessAsUser(dup, exe, "\"" + exe + "\" " + srv, IntPtr.Zero, IntPtr.Zero, false, NO_WINDOW, IntPtr.Zero, null, ref si, out pi);
      }
      if (ok) {
        Log("relaunched into session " + session + " (pid " + pi.pid + ", " + how + ")");
        CloseHandle(pi.process); CloseHandle(pi.thread);
      } else Log("relaunch: CreateProcessAsUser error " + Marshal.GetLastWin32Error());
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
      // Hardware health pushed over this same connection (metrics.rs), whether or not anyone is watching.
      var health = Task.Run(async () => {
        while (ws.State == WebSocketState.Open) {
          try { string j = BuildMetrics(); if (j != null) await SendText(ws, sendLock, j); } catch { }
          await Task.Delay(15000);
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
        if (op == "start") { Quality = Clamp(IntField(text, "q", 60), 20, 95); MaxW = IntField(text, "w", 0); WantH264 = Field(text, "codec") == "h264" ? 1 : 0; Codec = -1; if (_h264 != null) { _h264.Dispose(); _h264 = null; } fr.Prev = null; fr.Cur = new IntPtr(-1); streaming = true; Drain(frameReady); frameReady.Release(); }
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
      try { await health; } catch { }
    }
  }

  static async Task SendText(ClientWebSocket ws, SemaphoreSlim sendLock, string s) {
    var data = System.Text.Encoding.UTF8.GetBytes(s);
    await sendLock.WaitAsync();
    try { await ws.SendAsync(new ArraySegment<byte>(data), WebSocketMessageType.Text, true, CancellationToken.None); }
    finally { sendLock.Release(); }
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
    await SendCursor(ws, sendLock, fr);   // its shape changes without the screen changing (hovering a link)
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
      fr.Prev = cur;

      // First frame after a start: pick the codec (H.264 if asked and it initialises, else JPEG) and tell the viewer.
      if (Codec < 0) {
        Codec = 0;
        if (WantH264 == 1) {
          try { var enc = new H264(); if (enc.Init(w, h, 30, 6000000)) { _h264 = enc; Codec = 1; } else enc.Dispose(); } catch { }
        }
        await Send(ws, sendLock, new byte[] { 4, (byte)Codec }); // codec decision
        first = true;
      }

      if (Codec == 1) {
        // The encoder is sized to the screen: a resolution change re-creates it (a fresh IDR).
        if (_h264 != null && (_h264.W != w || _h264.H != h)) { _h264.Dispose(); _h264 = null; first = true;
          try { var enc = new H264(); if (enc.Init(w, h, 30, 6000000)) _h264 = enc; } catch { } }
        if (_h264 == null) { bmp.UnlockBits(data); Codec = 0; await Send(ws, sendLock, new byte[] { 4, 0 }); }
      }

      if (Codec == 1) {
        if (first) await Send(ws, sendLock, Header2(w, h));
        else if (top < 0) { bmp.UnlockBits(data); return false; } // nothing changed
        byte[] nv12 = ToNv12(data, w, h);
        bmp.UnlockBits(data);
        bool key; byte[] au = _h264.Encode(nv12, out key);
        if (au == null || au.Length == 0) return false;
        var msg = new byte[2 + au.Length];
        msg[0] = 3; msg[1] = (byte)((key ? 1 : 0));
        Buffer.BlockCopy(au, 0, msg, 2, au.Length);
        await Send(ws, sendLock, msg);
        return true;
      }

      // JPEG: send only the changed rectangle.
      bmp.UnlockBits(data);
      if (top < 0) return false;
      if (first) await Send(ws, sendLock, Header2(w, h));
      using (var sub = bmp.Clone(new Rectangle(0, top, w, bottom - top), PixelFormat.Format24bppRgb))
      using (var ms = new MemoryStream()) {
        sub.Save(ms, Jpeg(), JpegParams(Quality));
        await Send(ws, sendLock, FrameMsg(0, top, w, bottom - top, ms.ToArray()));
      }
      return true;
    }
  }

  // BGRA (our 32bpp capture) -> NV12 (what the H.264 encoder takes): a full Y plane then interleaved U,V at half size
  // (BT.601). unsafe for speed at 30 fps.
  static unsafe byte[] ToNv12(BitmapData data, int w, int h) {
    byte[] nv12 = new byte[w * h + ((w + 1) / 2) * ((h + 1) / 2) * 2];
    byte* src = (byte*)data.Scan0;
    int stride = data.Stride, uv = w * h;
    fixed (byte* dst = nv12) {
      for (int y = 0; y < h; y++) {
        byte* row = src + y * stride;
        byte* yrow = dst + y * w;
        for (int x = 0; x < w; x++) {
          byte b = row[x * 4], g = row[x * 4 + 1], r = row[x * 4 + 2];
          yrow[x] = (byte)(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
        }
      }
      int cw = (w + 1) / 2;
      for (int y = 0; y < h; y += 2) {
        byte* row = src + y * stride;
        byte* crow = dst + uv + (y / 2) * cw * 2;
        for (int x = 0; x < w; x += 2) {
          byte b = row[x * 4], g = row[x * 4 + 1], r = row[x * 4 + 2];
          int cx = x / 2;
          crow[cx * 2] = (byte)(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128);     // U
          crow[cx * 2 + 1] = (byte)(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128);  // V
        }
      }
    }
    return nv12;
  }

  static bool Same(byte[] a, byte[] b) {
    if (b == null || a.Length != b.Length) return false;
    for (int i = 0; i < a.Length; i++) if (a[i] != b[i]) return false;
    return true;
  }

  // The cursor drawn into the picture: wall thumbnails only. A controlling viewer gets its shape instead (SendCursor)
  // and the browser draws it: one cursor that follows the mouse at once, not a second one lagging a frame behind.
  static void DrawCursor(IntPtr hdc) {
    if (MaxW <= 0) return;
    var ci = new CURSORINFO(); ci.cbSize = Marshal.SizeOf(ci);
    if (!GetCursorInfo(ref ci) || ci.flags != 1 || ci.hCursor == IntPtr.Zero) return;
    int hx, hy; Hotspot(ci.hCursor, out hx, out hy);
    DrawIconEx(hdc, ci.x - VX - hx, ci.y - VY - hy, ci.hCursor, 0, 0, 0, IntPtr.Zero, DI_NORMAL);
  }

  // The point of the cursor image that is the mouse position (the arrow's tip, the I-beam's middle).
  static IntPtr _hotFor; static int _hotX, _hotY;
  static void Hotspot(IntPtr cur, out int hx, out int hy) {
    if (cur != _hotFor) {
      ICONINFO ii; _hotX = 0; _hotY = 0;
      if (GetIconInfo(cur, out ii)) {
        _hotX = ii.xHotspot; _hotY = ii.yHotspot;
        if (ii.hbmMask != IntPtr.Zero) DeleteObject(ii.hbmMask);
        if (ii.hbmColor != IntPtr.Zero) DeleteObject(ii.hbmColor);
      }
      _hotFor = cur;
    }
    hx = _hotX; hy = _hotY;
  }

  // Controlling viewer: the cursor's shape whenever it changes -> [5, hotspot x u16, y u16, PNG]; [5] alone = hidden
  // (a game that hides it: the browser hides its own too).
  static async Task SendCursor(ClientWebSocket ws, SemaphoreSlim sendLock, FrameState fr) {
    if (MaxW > 0) return;
    var ci = new CURSORINFO(); ci.cbSize = Marshal.SizeOf(ci);
    if (!GetCursorInfo(ref ci)) return;
    IntPtr cur = ci.flags == 1 ? ci.hCursor : IntPtr.Zero;
    if (cur == fr.Cur) return;
    fr.Cur = cur;
    byte[] msg = cur == IntPtr.Zero ? new byte[] { 5 } : CursorMsg(cur);
    if (msg != null) await Send(ws, sendLock, msg);
  }

  // The cursor as a PNG with transparency: drawn once on black and once on white, the difference is its opacity
  // (works for colour, alpha and old black-and-white cursors alike; pixels that invert the screen come out black).
  static byte[] CursorMsg(IntPtr cur) {
    int hx, hy; Hotspot(cur, out hx, out hy);
    const int S = 128;
    using (var black = new Bitmap(S, S, PixelFormat.Format32bppRgb))
    using (var white = new Bitmap(S, S, PixelFormat.Format32bppRgb)) {
      foreach (var p in new[] { new { B = black, C = Color.Black }, new { B = white, C = Color.White } })
        using (var g = Graphics.FromImage(p.B)) {
          g.Clear(p.C);
          IntPtr hdc = g.GetHdc();
          DrawIconEx(hdc, 0, 0, cur, 0, 0, 0, IntPtr.Zero, DI_NORMAL);
          g.ReleaseHdc(hdc);
        }
      int w = 1, h = 1;
      var px = new Color[S, S];
      for (int y = 0; y < S; y++)
        for (int x = 0; x < S; x++) {
          Color k = black.GetPixel(x, y), wt = white.GetPixel(x, y);
          int diff = ((wt.R - k.R) + (wt.G - k.G) + (wt.B - k.B)) / 3;
          Color c;
          if (diff < 0) c = Color.FromArgb(255, 0, 0, 0);           // inverts the screen
          else if (diff >= 255) c = Color.Transparent;
          else {
            int a = 255 - diff;
            c = Color.FromArgb(a, Math.Min(255, k.R * 255 / a), Math.Min(255, k.G * 255 / a), Math.Min(255, k.B * 255 / a));
          }
          px[x, y] = c;
          if (c.A > 0) { w = Math.Max(w, x + 1); h = Math.Max(h, y + 1); }
        }
      w = Math.Max(w, hx + 1); h = Math.Max(h, hy + 1);   // the hotspot must lie inside the image
      using (var outp = new Bitmap(w, h, PixelFormat.Format32bppArgb))
      using (var ms = new MemoryStream()) {
        for (int y = 0; y < h; y++)
          for (int x = 0; x < w; x++) outp.SetPixel(x, y, px[x, y]);
        outp.Save(ms, ImageFormat.Png);
        var png = ms.ToArray();
        var msg = new byte[5 + png.Length]; msg[0] = 5;
        U16(msg, 1, hx); U16(msg, 3, hy);
        Buffer.BlockCopy(png, 0, msg, 5, png.Length);
        return msg;
      }
    }
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

  // ---- hardware health (WMI) ----
  // Best-effort: every query in its own try/catch, a missing one just drops that field. Temperatures are ACPI thermal
  // zones (no kernel driver, so unlabeled, not per-core). Disk SMART gives bad-sector counts without a surface scan.
  static string BuildMetrics() {
    int cpu = -1, ram = -1;
    try {
      foreach (var o in Query(null, "SELECT PercentProcessorTime FROM Win32_PerfFormattedData_PerfOS_Processor WHERE Name='_Total'"))
        { cpu = (int)Convert.ToUInt64(o["PercentProcessorTime"]); break; }
    } catch { }
    try {
      foreach (var o in Query(null, "SELECT FreePhysicalMemory,TotalVisibleMemorySize FROM Win32_OperatingSystem")) {
        double free = Convert.ToDouble(o["FreePhysicalMemory"]), total = Convert.ToDouble(o["TotalVisibleMemorySize"]);
        if (total > 0) ram = (int)Math.Round(100.0 * (1.0 - free / total));
        break;
      }
    } catch { }
    if (cpu < 0 && ram < 0) return null;

    var temps = new List<int>();
    try {
      foreach (var o in Query(@"root\wmi", "SELECT CurrentTemperature FROM MSAcpi_ThermalZoneTemperature"))
        temps.Add((int)Math.Round(Convert.ToDouble(o["CurrentTemperature"]) / 10.0 - 273.15));
    } catch { }

    var sb = new System.Text.StringBuilder();
    sb.Append("{\"t\":\"metrics\",\"cpu\":").Append(Math.Max(cpu, 0)).Append(",\"ram\":").Append(Math.Max(ram, 0));
    sb.Append(",\"temps\":[");
    for (int i = 0; i < temps.Count; i++) { if (i > 0) sb.Append(','); sb.Append(temps[i]); }
    sb.Append("],\"disks\":[");
    sb.Append(Disks());
    sb.Append(']');
    ulong free2, total2, avail2;
    if (GetDiskFreeSpaceExW(Environment.GetFolderPath(Environment.SpecialFolder.System).Substring(0, 3), out avail2, out total2, out free2))
      sb.Append(",\"free\":").Append(free2);
    sb.Append('}');
    return sb.ToString();
  }

  static ManagementObjectCollection Query(string ns, string wql) {
    var scope = ns == null ? new ManagementScope() : new ManagementScope(@"\\.\" + ns);
    return new ManagementObjectSearcher(scope, new ObjectQuery(wql)).Get();
  }

  // One JSON object per physical drive: model + health from Win32_DiskDrive, bad-sector counts + temperature from its
  // SMART data (matched by the device instance path), predict-fail from the SMART status.
  static string Disks() {
    var parts = new List<string>();
    try {
      // SMART by instance name.
      var sraw = new Dictionary<string, uint[]>(StringComparer.OrdinalIgnoreCase); // name -> [realloc,pending,temp,have]
      try {
        foreach (var o in Query(@"root\wmi", "SELECT InstanceName,VendorSpecific FROM MSStorageDriver_FailurePredictData")) {
          var v = o["VendorSpecific"] as byte[]; if (v == null) continue;
          uint realloc = 0, pending = 0; int temp = -1000;
          for (int i = 2; i + 12 <= v.Length; i += 12) {
            int id = v[i]; if (id == 0) continue;
            uint raw = (uint)(v[i + 5] | (v[i + 6] << 8) | (v[i + 7] << 16) | (v[i + 8] << 24));
            if (id == 5) realloc = raw; else if (id == 197) pending = raw; else if (id == 194) temp = v[i + 5];
          }
          sraw[(string)o["InstanceName"]] = new uint[] { realloc, pending, (uint)(temp == -1000 ? 0xFFFF : temp), (uint)(temp == -1000 ? 0 : 1) };
        }
      } catch { }
      var sfail = new Dictionary<string, bool>(StringComparer.OrdinalIgnoreCase);
      try {
        foreach (var o in Query(@"root\wmi", "SELECT InstanceName,PredictFailure FROM MSStorageDriver_FailurePredictStatus"))
          sfail[(string)o["InstanceName"]] = Convert.ToBoolean(o["PredictFailure"]);
      } catch { }

      foreach (var d in Query(null, "SELECT Model,Status,PNPDeviceID FROM Win32_DiskDrive")) {
        string name = (d["Model"] as string ?? "disk").Trim();
        string health = (d["Status"] as string ?? "").Trim(); // "OK", "Pred Fail", "Degraded"...
        string pnp = (d["PNPDeviceID"] as string ?? "").Replace("\\", "_");
        uint[] s = null; bool pf = false;
        foreach (var kv in sraw) { var k = kv.Key.Replace("\\", "_"); if (pnp.Length > 0 && (k.IndexOf(pnp, StringComparison.OrdinalIgnoreCase) >= 0 || pnp.IndexOf(k, StringComparison.OrdinalIgnoreCase) >= 0)) { s = kv.Value; break; } }
        foreach (var kv in sfail) { var k = kv.Key.Replace("\\", "_"); if (pnp.Length > 0 && (k.IndexOf(pnp, StringComparison.OrdinalIgnoreCase) >= 0 || pnp.IndexOf(k, StringComparison.OrdinalIgnoreCase) >= 0)) { pf = kv.Value; break; } }
        var o = new System.Text.StringBuilder();
        o.Append("{\"name\":\"").Append(JsonEsc(name)).Append("\",\"health\":\"").Append(JsonEsc(health)).Append('"');
        if (s != null) {
          o.Append(",\"realloc\":").Append(s[0]).Append(",\"pending\":").Append(s[1]);
          if (s[3] == 1) o.Append(",\"temp\":").Append((short)s[2]);
        }
        if (pf) o.Append(",\"predict_fail\":true");
        o.Append('}');
        parts.Add(o.ToString());
      }
    } catch { }
    return string.Join(",", parts.ToArray());
  }

  static string JsonEsc(string s) {
    var b = new System.Text.StringBuilder();
    foreach (char c in s) {
      if (c == '"' || c == '\\') b.Append('\\').Append(c);
      else if (c >= ' ' && c < 127) b.Append(c);
      else b.Append(' '); // keep it ASCII and valid
    }
    return b.ToString();
  }

  [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern bool GetDiskFreeSpaceExW(string dir, out ulong avail, out ulong total, out ulong free);

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

  // ---- H.264 encoding (Media Foundation) ----
  // The built-in software H.264 encoder MFT. NV12 in, an Annex-B H.264 access unit out, which the browser plays with
  // WebCodecs. Called by vtable index like the DXGI interop. Falls back to JPEG (the caller) if any step fails.
  [DllImport("mfplat.dll")] static extern int MFStartup(uint version, uint flags);
  [DllImport("mfplat.dll")] static extern int MFShutdown();
  [DllImport("mfplat.dll")] static extern int MFCreateMediaType(out IntPtr type);
  [DllImport("mfplat.dll")] static extern int MFCreateSample(out IntPtr sample);
  [DllImport("mfplat.dll")] static extern int MFCreateMemoryBuffer(uint maxLen, out IntPtr buffer);
  [DllImport("ole32.dll")] static extern int CoCreateInstance(ref Guid clsid, IntPtr outer, uint ctx, ref Guid iid, out IntPtr ptr);

  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int SetGuidFn(IntPtr self, ref Guid key, ref Guid val);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int SetU32Fn(IntPtr self, ref Guid key, uint val);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int SetU64Fn(IntPtr self, ref Guid key, ulong val);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int GetU32Fn(IntPtr self, ref Guid key, out uint val);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int SetTypeFn(IntPtr self, uint stream, IntPtr type, uint flags);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int MsgFn(IntPtr self, uint msg, IntPtr param);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int ProcessInputFn(IntPtr self, uint stream, IntPtr sample, uint flags);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int ProcessOutputFn(IntPtr self, uint flags, uint count, ref MFT_OUTPUT_DATA_BUFFER buf, out uint status);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int OutInfoFn(IntPtr self, uint stream, out MFT_OUTPUT_STREAM_INFO info);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int SetTimeFn(IntPtr self, long t);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int AddBufFn(IntPtr self, IntPtr buf);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int GetBufFn(IntPtr self, uint i, out IntPtr buf);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int LockFn(IntPtr self, out IntPtr ptr, out uint max, out uint cur);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int UnlockFn(IntPtr self);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int SetLenFn(IntPtr self, uint len);

  [StructLayout(LayoutKind.Sequential)] struct MFT_OUTPUT_DATA_BUFFER { public uint dwStreamID; public IntPtr pSample; public uint dwStatus; public IntPtr pEvents; }
  [StructLayout(LayoutKind.Sequential)] struct MFT_OUTPUT_STREAM_INFO { public uint dwFlags, cbSize, cbAlignment; }

  static Guid CLSID_H264Enc = new Guid("6ca50344-051a-4ded-9779-a43305165e35");
  static Guid IID_IMFTransform = new Guid("bf94c121-5b05-4e6f-8000-ba598961414d");
  static Guid MFMediaType_Video = new Guid("73646976-0000-0010-8000-00aa00389b71");
  static Guid MFVideoFormat_H264 = new Guid("34363248-0000-0010-8000-00aa00389b71");
  static Guid MFVideoFormat_NV12 = new Guid("3231564e-0000-0010-8000-00aa00389b71");
  static Guid MF_MT_MAJOR_TYPE = new Guid("48eba18e-f8c9-4687-bf11-0a74c9f96a8f");
  static Guid MF_MT_SUBTYPE = new Guid("f7e34c9a-42e8-4714-b74b-cb29d72c35e5");
  static Guid MF_MT_AVG_BITRATE = new Guid("20332624-fb0d-4d9e-bd0d-cbf6786c102e");
  static Guid MF_MT_FRAME_SIZE = new Guid("1652c33d-d6b2-4012-b834-72030849a37d");
  static Guid MF_MT_FRAME_RATE = new Guid("c459a2e8-3d2c-4e44-b132-fee5156c7bb0");
  static Guid MF_MT_PIXEL_ASPECT_RATIO = new Guid("c6376a1e-8d0a-4027-be45-6d9a0ad39bb6");
  static Guid MF_MT_INTERLACE_MODE = new Guid("e2724bb8-e676-40c6-a4d8-1c4a3f0987c9");
  static Guid MF_MT_MPEG2_PROFILE = new Guid("ad76a80b-2d5c-4e0b-b375-64e520137036");
  static Guid MF_CLEANPOINT = new Guid("9cdf01d8-a0f0-43ba-b077-eaa06cbd728a");
  const int MF_E_TRANSFORM_NEED_MORE_INPUT = unchecked((int)0xC00D6D72);

  static int _mfStarted;
  class H264 : IDisposable {
    IntPtr mft;
    long time;
    uint outSize;
    public int W, H, Fps;

    public bool Init(int w, int h, int fps, int bitrate) {
      if (System.Threading.Interlocked.Exchange(ref _mfStarted, 1) == 0 && MFStartup(0x00020070, 1) != 0) return false;
      W = w; H = h; Fps = fps;
      Guid clsid = CLSID_H264Enc, iid = IID_IMFTransform;
      if (CoCreateInstance(ref clsid, IntPtr.Zero, 1, ref iid, out mft) != 0 || mft == IntPtr.Zero) return false;
      ulong size = ((ulong)(uint)w << 32) | (uint)h, rate = ((ulong)(uint)fps << 32) | 1, par = ((ulong)1 << 32) | 1;
      IntPtr ot;
      if (MFCreateMediaType(out ot) != 0) return false;
      SetG(ot, MF_MT_MAJOR_TYPE, MFMediaType_Video); SetG(ot, MF_MT_SUBTYPE, MFVideoFormat_H264);
      SetU(ot, MF_MT_AVG_BITRATE, (uint)bitrate); SetU(ot, MF_MT_INTERLACE_MODE, 2); SetU(ot, MF_MT_MPEG2_PROFILE, 66);
      Set8(ot, MF_MT_FRAME_SIZE, size); Set8(ot, MF_MT_FRAME_RATE, rate); Set8(ot, MF_MT_PIXEL_ASPECT_RATIO, par);
      int hr = Vt<SetTypeFn>(mft, 16)(mft, 0, ot, 0); Rel(ot); if (hr != 0) return false;
      IntPtr it;
      if (MFCreateMediaType(out it) != 0) return false;
      SetG(it, MF_MT_MAJOR_TYPE, MFMediaType_Video); SetG(it, MF_MT_SUBTYPE, MFVideoFormat_NV12);
      SetU(it, MF_MT_INTERLACE_MODE, 2);
      Set8(it, MF_MT_FRAME_SIZE, size); Set8(it, MF_MT_FRAME_RATE, rate); Set8(it, MF_MT_PIXEL_ASPECT_RATIO, par);
      hr = Vt<SetTypeFn>(mft, 15)(mft, 0, it, 0); Rel(it); if (hr != 0) return false;
      MFT_OUTPUT_STREAM_INFO osi; Vt<OutInfoFn>(mft, 7)(mft, 0, out osi);
      outSize = osi.cbSize > 0 ? osi.cbSize : (uint)(w * h);
      Vt<MsgFn>(mft, 23)(mft, 0x10000000, IntPtr.Zero); // BEGIN_STREAMING
      Vt<MsgFn>(mft, 23)(mft, 0x10000002, IntPtr.Zero); // START_OF_STREAM
      return true;
    }

    static void SetG(IntPtr t, Guid k, Guid v) { Vt<SetGuidFn>(t, 24)(t, ref k, ref v); }
    static void SetU(IntPtr t, Guid k, uint v) { Vt<SetU32Fn>(t, 21)(t, ref k, v); }
    static void Set8(IntPtr t, Guid k, ulong v) { Vt<SetU64Fn>(t, 22)(t, ref k, v); }

    public byte[] Encode(byte[] nv12, out bool key) {
      key = false;
      IntPtr buf, sample, p; uint max, cur;
      if (MFCreateMemoryBuffer((uint)nv12.Length, out buf) != 0) return null;
      Vt<LockFn>(buf, 3)(buf, out p, out max, out cur);
      Marshal.Copy(nv12, 0, p, nv12.Length);
      Vt<UnlockFn>(buf, 4)(buf);
      Vt<SetLenFn>(buf, 6)(buf, (uint)nv12.Length);
      MFCreateSample(out sample);
      Vt<AddBufFn>(sample, 37)(sample, buf);
      long dur = 10000000L / Fps;
      Vt<SetTimeFn>(sample, 31)(sample, time); Vt<SetTimeFn>(sample, 33)(sample, dur); time += dur;
      int hr = Vt<ProcessInputFn>(mft, 24)(mft, 0, sample, 0);
      Rel(buf); Rel(sample);
      if (hr != 0) return null;

      IntPtr obuf, osample, op; uint omax, ocur, status;
      if (MFCreateMemoryBuffer(outSize, out obuf) != 0) return null;
      MFCreateSample(out osample);
      Vt<AddBufFn>(osample, 37)(osample, obuf);
      var odb = new MFT_OUTPUT_DATA_BUFFER { pSample = osample };
      hr = Vt<ProcessOutputFn>(mft, 25)(mft, 0, 1, ref odb, out status);
      byte[] outb = null;
      if (hr == 0) {
        uint ck; Guid kg = MF_CLEANPOINT;
        key = Vt<GetU32Fn>(osample, 7)(osample, ref kg, out ck) == 0 && ck != 0;
        IntPtr cb; Vt<GetBufFn>(osample, 35)(osample, 0, out cb);
        Vt<LockFn>(cb, 3)(cb, out op, out omax, out ocur);
        outb = new byte[ocur];
        Marshal.Copy(op, outb, 0, (int)ocur);
        Vt<UnlockFn>(cb, 4)(cb);
        Rel(cb);
      } // else NEED_MORE_INPUT: no access unit this time
      Rel(obuf); Rel(osample);
      return outb;
    }

    public void Dispose() { Rel(mft); mft = IntPtr.Zero; }
  }
}

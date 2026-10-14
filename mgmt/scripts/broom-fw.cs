// broom-fw.cs - UEFI BootOrder access for broom-bootorder.ps1 (PowerShell cannot call these Win32 functions itself).
// Compiled to broom-fw.dll by CI (Windows runner, .NET Framework 4 csc = what Add-Type uses) and shipped in the
// release's assets; a local build ships only this source and the client compiles it once per boot. ASCII only.
using System; using System.Runtime.InteropServices;
public static class BroomFw {
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  static extern uint GetFirmwareEnvironmentVariableExW(string name, string guid, byte[] buf, uint size, IntPtr attr);
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  static extern bool SetFirmwareEnvironmentVariableExW(string name, string guid, byte[] buf, uint size, uint attr);
  [DllImport("advapi32.dll", SetLastError=true)]
  static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
  [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  static extern bool LookupPrivilegeValueW(string system, string name, out long luid);
  [DllImport("advapi32.dll", SetLastError=true)]
  static extern bool AdjustTokenPrivileges(IntPtr token, bool disableAll, ref TokenPriv tp, uint len, IntPtr prev, IntPtr retLen);
  [DllImport("kernel32.dll")]
  static extern IntPtr GetCurrentProcess();
  [StructLayout(LayoutKind.Sequential, Pack = 4)]
  struct TokenPriv { public uint Count; public long Luid; public uint Attr; }
  const string EfiGlobal = "{8BE4DF61-93CA-11D2-AA0D-00E098032B8C}";
  public static void EnablePrivilege() {
    IntPtr t; OpenProcessToken(GetCurrentProcess(), 0x28, out t); // TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY
    TokenPriv tp = new TokenPriv(); tp.Count = 1; tp.Attr = 2;    // SE_PRIVILEGE_ENABLED
    LookupPrivilegeValueW(null, "SeSystemEnvironmentPrivilege", out tp.Luid);
    AdjustTokenPrivileges(t, false, ref tp, 0, IntPtr.Zero, IntPtr.Zero);
  }
  public static byte[] Get(string name) {
    byte[] b = new byte[4096];
    uint n = GetFirmwareEnvironmentVariableExW(name, EfiGlobal, b, (uint)b.Length, IntPtr.Zero);
    if (n == 0) return null;
    byte[] r = new byte[n]; Array.Copy(b, r, n); return r;
  }
  // 7 = NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS (the attributes BootOrder has)
  public static bool Set(string name, byte[] data) { return SetFirmwareEnvironmentVariableExW(name, EfiGlobal, data, (uint)data.Length, 7); }
}

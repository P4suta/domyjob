using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading;
using Microsoft.Win32.SafeHandles;

public static class DomyjobBootstrapInput
{
    private const int StandardInputHandle = -10;
    private const int ErrorIoPending = 997;
    private const int ErrorBrokenPipe = 109;
    private const int ErrorHandleEof = 38;
    private const int MaximumSourceBytes = @SOURCE_MAX@;
    private const uint FileTypePipe = 3;

    [StructLayout(LayoutKind.Sequential)]
    private struct Overlapped
    {
        public UIntPtr Internal;
        public UIntPtr InternalHigh;
        public uint Offset;
        public uint OffsetHigh;
        public IntPtr Event;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr GetStdHandle(int standardHandle);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint GetFileType(SafeFileHandle input);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool ReadFile(SafeFileHandle input, IntPtr destination,
        uint count, IntPtr received, IntPtr operation);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetOverlappedResult(SafeFileHandle input,
        IntPtr operation, out uint received, [MarshalAs(UnmanagedType.Bool)] bool wait);

    public static byte[] ReadBytes(int count)
    {
        return ReadBytes(count, null);
    }

    public static byte[] ReadBytes(int count, Action onPending)
    {
        if (count < 0 || count > MaximumSourceBytes)
            throw new ArgumentOutOfRangeException("count");
        byte[] bytes = new byte[count];
        if (count == 0)
            return bytes;
        using (SafeFileHandle input = new SafeFileHandle(GetStdHandle(StandardInputHandle), false))
        using (ManualResetEvent completed = new ManualResetEvent(false))
        {
            if (input.IsInvalid)
                throw new IOException("bootstrap stdin is not available");
            if (GetFileType(input) != FileTypePipe)
                throw new IOException("bootstrap stdin must be a pipe");
            GCHandle pinned = GCHandle.Alloc(bytes, GCHandleType.Pinned);
            IntPtr operation = IntPtr.Zero;
            try
            {
                operation = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(Overlapped)));
                int offset = 0;
                while (offset < count)
                {
                    Overlapped state = new Overlapped();
                    state.Event = completed.SafeWaitHandle.DangerousGetHandle();
                    Marshal.StructureToPtr(state, operation, false);
                    uint remaining = checked((uint)(count - offset));
                    bool started = ReadFile(input, IntPtr.Add(pinned.AddrOfPinnedObject(), offset),
                        remaining, IntPtr.Zero, operation);
                    int readError = started ? 0 : Marshal.GetLastWin32Error();
                    Exception observerError = null;
                    if (!started)
                    {
                        if (readError != ErrorIoPending)
                            throw ReadError(readError);
                        if (onPending != null)
                        {
                            try { onPending(); }
                            catch (Exception errorDuringObservation) { observerError = errorDuringObservation; }
                        }
                    }
                    uint received;
                    if (!GetOverlappedResult(input, operation, out received, true))
                        throw ReadError(Marshal.GetLastWin32Error());
                    if (received > remaining)
                        throw new IOException("bootstrap read exceeded the requested prefix");
                    if (observerError != null)
                        throw observerError;
                    if (received == 0)
                        continue;
                    offset = checked(offset + (int)received);
                }
                return bytes;
            }
            finally
            {
                pinned.Free();
                if (operation != IntPtr.Zero)
                    Marshal.FreeHGlobal(operation);
            }
        }
    }

    private static Exception ReadError(int error)
    {
        if (error == ErrorBrokenPipe || error == ErrorHandleEof)
            return new EndOfStreamException("truncated bootstrap source");
        return new Win32Exception(error);
    }
}

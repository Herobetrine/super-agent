"""Single scheduler ownership and Windows kill-on-close process containment."""
import ctypes
from ctypes import wintypes

_handles = []


def claim(directory,title):
    import msvcrt
    lock = open(directory/"desktop.lock","a+b")
    lock.seek(0)
    try:
        msvcrt.locking(lock.fileno(),msvcrt.LK_NBLCK,1)
    except OSError:
        lock.close()
        user=ctypes.WinDLL("user32",use_last_error=True)
        user.FindWindowW.argtypes=[wintypes.LPCWSTR,wintypes.LPCWSTR]
        user.FindWindowW.restype=wintypes.HWND
        window=user.FindWindowW(None,title)
        if window:
            user.ShowWindow.argtypes=[wintypes.HWND,ctypes.c_int]
            user.SetForegroundWindow.argtypes=[wintypes.HWND]
            user.ShowWindow(window,9)
            user.SetForegroundWindow(window)
        return False
    _handles.append(lock)
    # Child CLI processes inherit job membership; a crashed UI cannot leave local writers running.
    k=ctypes.WinDLL("kernel32",use_last_error=True)
    class Basic(ctypes.Structure):
        _fields_=[("process_time",ctypes.c_int64),("job_time",ctypes.c_int64),
            ("flags",wintypes.DWORD),("min_working",ctypes.c_size_t),("max_working",ctypes.c_size_t),
            ("active",wintypes.DWORD),("affinity",ctypes.c_size_t),("priority",wintypes.DWORD),
            ("scheduling",wintypes.DWORD)]
    class Extended(ctypes.Structure):
        _fields_=[("basic",Basic),("io",ctypes.c_uint64*6),
                  ("memory",ctypes.c_size_t*4)]
    k.CreateJobObjectW.argtypes=[ctypes.c_void_p,wintypes.LPCWSTR]
    k.CreateJobObjectW.restype=wintypes.HANDLE
    k.SetInformationJobObject.argtypes=[wintypes.HANDLE,ctypes.c_int,ctypes.c_void_p,wintypes.DWORD]
    k.AssignProcessToJobObject.argtypes=[wintypes.HANDLE,wintypes.HANDLE]
    k.GetCurrentProcess.restype=wintypes.HANDLE
    job=k.CreateJobObjectW(None,None)
    info=Extended()
    info.basic.flags=0x2000
    if not (job and k.SetInformationJobObject(job,9,ctypes.byref(info),ctypes.sizeof(info))
            and k.AssignProcessToJobObject(job,k.GetCurrentProcess())):
        raise ctypes.WinError(ctypes.get_last_error())
    _handles.append(job)
    return True

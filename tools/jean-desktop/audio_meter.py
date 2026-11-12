"""Read active Windows playback endpoint levels without opening an audio stream."""

import ctypes
import time
import uuid

HRESULT = ctypes.c_long
POINTER = ctypes.c_void_p


def guid(value):
    return (ctypes.c_ubyte * 16).from_buffer_copy(uuid.UUID(value).bytes_le)


DEVICE_ENUMERATOR = guid("BCDE0395-E52F-467C-8E3D-C4579291692E")
IID_ENUMERATOR = guid("A95664D2-9614-4F35-A746-DE8DB63617E6")
IID_METER = guid("C02216F6-8C67-4B5B-9D00-D008E73E0064")

class PropertyKey(ctypes.Structure):
    _fields_ = [("fmtid", ctypes.c_ubyte * 16), ("pid", ctypes.c_ulong)]


class PropertyValue(ctypes.Structure):
    # PROPVARIANT reserves room for a counted array (length and pointer).
    _fields_ = [("vt", ctypes.c_ushort), ("reserved", ctypes.c_ushort * 3),
                ("value", POINTER), ("padding", POINTER)]


FORM_FACTOR = PropertyKey(guid("1DA5D803-D492-4EDD-8C23-E0C0FFEE7F0E"), 0)
FRIENDLY_NAME = PropertyKey(guid("A45C254E-DF1C-4EFD-8020-67D146A850E0"), 14)


def method(interface, slot, result, *parameters):
    vtable = ctypes.cast(interface, ctypes.POINTER(ctypes.POINTER(POINTER)))[0]
    return ctypes.WINFUNCTYPE(result, POINTER, *parameters)(vtable[slot])


def release(interface):
    if interface:
        method(interface, 2, ctypes.c_ulong)(interface)


class PlaybackMeter:
    """Use from the GUI thread and release on that thread at shutdown."""

    def __init__(self):
        self.ole = ctypes.WinDLL("ole32")
        self.ole.CoInitializeEx.argtypes = (POINTER, ctypes.c_ulong)
        self.ole.CoInitializeEx.restype = HRESULT
        self.ole.CoCreateInstance.argtypes = (
            POINTER, POINTER, ctypes.c_ulong, POINTER, ctypes.POINTER(POINTER),
        )
        self.ole.CoCreateInstance.restype = HRESULT
        self.ole.CoUninitialize.restype = None
        self.enumerator = POINTER()
        self.meters = None
        self.refresh_at = 0.0
        self.initialized = False
        result = self.ole.CoInitializeEx(None, 2)  # COINIT_APARTMENTTHREADED
        self.initialized = result >= 0
        if not self.initialized and (result & 0xFFFFFFFF) != 0x80010106:
            raise OSError(f"Core Audio COM initialization failed ({result:#x})")
        result = self.ole.CoCreateInstance(
            ctypes.byref(DEVICE_ENUMERATOR), None, 1,
            ctypes.byref(IID_ENUMERATOR), ctypes.byref(self.enumerator),
        )
        if result < 0:
            self.close()
            raise OSError(f"Core Audio device enumeration failed ({result:#x})")

    def _refresh(self):
        collection = POINTER()
        meters = []
        verified = False
        try:
            enumerate_devices = method(self.enumerator, 3, HRESULT, ctypes.c_int,
                                       ctypes.c_ulong, ctypes.POINTER(POINTER))
            if enumerate_devices(self.enumerator, 0, 1, ctypes.byref(collection)) < 0:
                return
            count = ctypes.c_uint()
            get_count = method(collection, 3, HRESULT, ctypes.POINTER(ctypes.c_uint))
            if get_count(collection, ctypes.byref(count)) < 0:
                return
            item = method(collection, 4, HRESULT, ctypes.c_uint, ctypes.POINTER(POINTER))
            for index in range(count.value):
                device, meter = POINTER(), POINTER()
                if item(collection, index, ctypes.byref(device)) < 0:
                    return
                try:
                    activate = method(device, 3, HRESULT, POINTER, ctypes.c_ulong,
                                      POINTER, ctypes.POINTER(POINTER))
                    if activate(device, ctypes.byref(IID_METER), 1, None, ctypes.byref(meter)) < 0:
                        return
                    meters.append(meter)
                finally:
                    release(device)
            verified = True
        finally:
            release(collection)
            for meter in self.meters or ():
                release(meter)
            self.meters = meters if verified else None
            if not verified:
                for meter in meters:
                    release(meter)

    def peak(self):
        now = time.monotonic()
        if now >= self.refresh_at:
            self._refresh()
            self.refresh_at = now + 3
        if self.meters is None:
            return None
        peak = 0.0
        for meter in self.meters:
            value = ctypes.c_float()
            read = method(meter, 3, HRESULT, ctypes.POINTER(ctypes.c_float))
            if read(meter, ctypes.byref(value)) < 0:
                self.refresh_at = 0.0
                return None
            peak = max(peak, value.value)
        return min(1.0, peak)

    def microphone(self, *, verify_all_roles=False):
        """Verify the capture endpoint. Wake uses Windows' default-device mapper."""
        names = set()
        get_default = method(self.enumerator, 4, HRESULT, ctypes.c_int,
                             ctypes.c_int, ctypes.POINTER(POINTER))
        for role in ((0, 1, 2) if verify_all_roles else (1,)):
            device, store = POINTER(), POINTER()
            try:
                if get_default(self.enumerator, 1, role, ctypes.byref(device)) < 0:
                    raise OSError("No default Windows microphone is available.")
                open_store = method(device, 4, HRESULT, ctypes.c_ulong,
                                    ctypes.POINTER(POINTER))
                if open_store(device, 0, ctypes.byref(store)) < 0:
                    raise OSError("Could not verify the default Windows microphone.")
                read = method(store, 5, HRESULT, ctypes.POINTER(PropertyKey),
                              ctypes.POINTER(PropertyValue))
                name = PropertyValue()
                form = PropertyValue()
                try:
                    if (read(store, ctypes.byref(FORM_FACTOR), ctypes.byref(form)) < 0
                            or form.vt != 19 or form.value not in (4, 5, 6)):
                        raise OSError("Set a microphone or headset mic as the Windows input.")
                    if (read(store, ctypes.byref(FRIENDLY_NAME), ctypes.byref(name)) < 0
                            or name.vt != 31 or not name.value):
                        raise OSError("Could not identify the Windows microphone.")
                    names.add(ctypes.wstring_at(name.value))
                finally:
                    self.ole.PropVariantClear(ctypes.byref(name))
                    self.ole.PropVariantClear(ctypes.byref(form))
            finally:
                release(store)
                release(device)
        if len(names) != 1:
            raise OSError("Set the same Windows microphone as default input and communications input.")
        return names.pop()

    def close(self):
        for meter in self.meters or ():
            release(meter)
        release(self.enumerator)
        self.meters = None
        self.enumerator = POINTER()
        if self.initialized:
            self.ole.CoUninitialize()
            self.initialized = False

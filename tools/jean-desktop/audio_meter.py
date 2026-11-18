"""Read active Windows playback endpoint levels without opening an audio stream."""

import ctypes
import re
import time
import uuid
from contextlib import contextmanager

HRESULT = ctypes.c_long
POINTER = ctypes.c_void_p

# Windows Core Audio EndpointFormFactor values allowed for physical/USB/Bluetooth capture inputs:
# 2=LineLevel (USB interfaces), 3=Headphones (Bluetooth profiles), 4=Microphone, 5=Headset,
# 6=Handset, 10=UnknownFormFactor (composite USB and Bluetooth drivers).
# Excludes 0=RemoteNetworkDevice, 1=Speakers, 7=UnknownDigitalPassthrough, 8=SPDIF, 9=DigitalAudioDisplayDevice.
CAPTURE_FORMS = {2, 3, 4, 5, 6, 10}
LOOPBACK_NAME = re.compile(
    r"\b(?:stereo mix|what u hear|wave\s*out|loopback)\b", re.IGNORECASE
)


def is_capture_input(form_vt, form_value, friendly_name):
    if not friendly_name or LOOPBACK_NAME.search(friendly_name):
        return False
    return form_vt == 19 and form_value in CAPTURE_FORMS


def guid(value):
    return (ctypes.c_ubyte * 16).from_buffer_copy(uuid.UUID(value).bytes_le)


DEVICE_ENUMERATOR = guid("BCDE0395-E52F-467C-8E3D-C4579291692E")
IID_ENUMERATOR = guid("A95664D2-9614-4F35-A746-DE8DB63617E6")
IID_METER = guid("C02216F6-8C67-4B5B-9D00-D008E73E0064")
IID_VOLUME = guid("5CDF2C82-841E-4546-9722-0CF74078229A")
POLICY_CONFIG = guid("870AF99C-171D-4F9E-AF0D-E63DF40C2BC9")
IID_POLICY_CONFIG = guid("F8679F50-850A-41CF-9C72-430F290290C8")

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
        self.complete = False
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
        enumerated = False
        complete = True
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
                    complete = False
                    continue
                try:
                    activate = method(device, 3, HRESULT, POINTER, ctypes.c_ulong,
                                      POINTER, ctypes.POINTER(POINTER))
                    if activate(device, ctypes.byref(IID_METER), 1, None, ctypes.byref(meter)) < 0:
                        complete = False
                        continue
                    meters.append(meter)
                finally:
                    release(device)
            enumerated = True
        finally:
            release(collection)
            for meter in self.meters or ():
                release(meter)
            self.meters = meters if enumerated else None
            self.complete = enumerated and complete
            if not enumerated:
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
        read_failed = False
        for meter in self.meters:
            value = ctypes.c_float()
            read = method(meter, 3, HRESULT, ctypes.POINTER(ctypes.c_float))
            if read(meter, ctypes.byref(value)) < 0:
                self.refresh_at = 0.0
                read_failed = True
                continue
            peak = max(peak, value.value)
        if peak > 0.0:
            return min(1.0, peak)
        if not self.complete or read_failed:
            return None
        return 0.0

    def _endpoint_id(self, device):
        value = POINTER()
        try:
            if method(device, 5, HRESULT, ctypes.POINTER(POINTER))(device, ctypes.byref(value)) < 0 or not value:
                raise OSError("Windows could not identify the audio endpoint.")
            return ctypes.wstring_at(value)
        finally:
            self.ole.CoTaskMemFree.argtypes = (POINTER,)
            self.ole.CoTaskMemFree(value)

    def default_endpoint(self, capture, role=1):
        device = POINTER()
        try:
            get_default = method(self.enumerator, 4, HRESULT, ctypes.c_int,
                                 ctypes.c_int, ctypes.POINTER(POINTER))
            if get_default(self.enumerator, int(capture), role, ctypes.byref(device)) < 0:
                return None
            return self._endpoint_id(device)
        finally:
            release(device)

    def endpoints(self, capture):
        collection = POINTER()
        endpoints = []
        try:
            enumerate_devices = method(self.enumerator, 3, HRESULT, ctypes.c_int,
                                       ctypes.c_ulong, ctypes.POINTER(POINTER))
            if enumerate_devices(self.enumerator, int(capture), 1, ctypes.byref(collection)) < 0:
                raise OSError("Windows could not list audio devices.")
            count = ctypes.c_uint()
            if method(collection, 3, HRESULT, ctypes.POINTER(ctypes.c_uint))(collection, ctypes.byref(count)) < 0:
                raise OSError("Windows could not count audio devices.")
            item = method(collection, 4, HRESULT, ctypes.c_uint, ctypes.POINTER(POINTER))
            for index in range(count.value):
                device, store = POINTER(), POINTER()
                name = PropertyValue()
                form = PropertyValue()
                try:
                    if item(collection, index, ctypes.byref(device)) < 0:
                        continue
                    if method(device, 4, HRESULT, ctypes.c_ulong, ctypes.POINTER(POINTER))(
                            device, 0, ctypes.byref(store)) < 0:
                        continue
                    read = method(store, 5, HRESULT, ctypes.POINTER(PropertyKey), ctypes.POINTER(PropertyValue))
                    if (read(store, ctypes.byref(FRIENDLY_NAME), ctypes.byref(name)) < 0
                            or name.vt != 31 or not name.value):
                        continue
                    friendly = ctypes.wstring_at(name.value)
                    if capture and (read(store, ctypes.byref(FORM_FACTOR), ctypes.byref(form)) < 0
                                    or not is_capture_input(form.vt, form.value, friendly)):
                        continue
                    endpoints.append((self._endpoint_id(device), friendly))
                finally:
                    self.ole.PropVariantClear(ctypes.byref(name))
                    self.ole.PropVariantClear(ctypes.byref(form))
                    release(store)
                    release(device)
            return endpoints
        finally:
            release(collection)

    def set_default_endpoint(self, capture, endpoint, *, roles=(0, 1, 2)):
        if endpoint not in dict(self.endpoints(capture)):
            raise OSError("This audio device is no longer available.")
        previous = {role: self.default_endpoint(capture, role) for role in roles}
        policy = POINTER()
        try:
            if self.ole.CoCreateInstance(ctypes.byref(POLICY_CONFIG), None, 1,
                                         ctypes.byref(IID_POLICY_CONFIG), ctypes.byref(policy)) < 0:
                raise OSError("Windows could not switch devices here. Use Windows audio devices.")
            set_default = method(policy, 13, HRESULT, ctypes.c_wchar_p, ctypes.c_int)
            for role in roles:
                if set_default(policy, endpoint, role) < 0:
                    for old_role, old_device in previous.items():
                        if old_device:
                            set_default(policy, old_device, old_role)
                    raise OSError("Windows rejected the device change. Check the current audio selection.")
        finally:
            release(policy)
            self.refresh_at = 0.0

    def microphone(self, *, verify_all_roles=False):
        """Verify the capture endpoint. Wake uses Windows' default-device mapper."""
        names = {}
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
                    if (read(store, ctypes.byref(FRIENDLY_NAME), ctypes.byref(name)) < 0
                            or name.vt != 31 or not name.value):
                        raise OSError("Could not identify the Windows microphone.")
                    friendly = ctypes.wstring_at(name.value)
                    if (read(store, ctypes.byref(FORM_FACTOR), ctypes.byref(form)) < 0
                            or not is_capture_input(form.vt, form.value, friendly)):
                        raise OSError("Set a microphone or headset mic as the Windows input.")
                    names[role] = friendly
                finally:
                    self.ole.PropVariantClear(ctypes.byref(name))
                    self.ole.PropVariantClear(ctypes.byref(form))
            finally:
                release(store)
                release(device)
        if len(set(names.values())) != 1:
            raise OSError(
                f"Windows input uses {names[1]}, communications uses {names[2]}, "
                f"and console input uses {names[0]}. In Audio → Windows audio devices → "
                "Recording, set your microphone as both Default Device and Default "
                "Communication Device, then turn Wake on."
            )
        return names[1]

    @contextmanager
    def _default_volume(self, capture):
        device, volume = POINTER(), POINTER()
        try:
            get_default = method(self.enumerator, 4, HRESULT, ctypes.c_int,
                                 ctypes.c_int, ctypes.POINTER(POINTER))
            if get_default(self.enumerator, int(capture), 1, ctypes.byref(device)) < 0:
                raise OSError("No default Windows audio device.")
            activate = method(device, 3, HRESULT, POINTER, ctypes.c_ulong,
                              POINTER, ctypes.POINTER(POINTER))
            if activate(device, ctypes.byref(IID_VOLUME), 1, None, ctypes.byref(volume)) < 0:
                raise OSError("Windows could not open the volume controls.")
            yield volume
        finally:
            release(volume)
            release(device)

    def volume_state(self, capture):
        level, muted = ctypes.c_float(), ctypes.c_int()
        with self._default_volume(capture) as volume:
            get_level = method(volume, 9, HRESULT, ctypes.POINTER(ctypes.c_float))
            get_mute = method(volume, 15, HRESULT, ctypes.POINTER(ctypes.c_int))
            if get_level(volume, ctypes.byref(level)) < 0 or get_mute(volume, ctypes.byref(muted)) < 0:
                raise OSError("Windows could not read the audio level.")
        return round(level.value * 100), bool(muted.value)

    def set_volume(self, capture, percent):
        with self._default_volume(capture) as volume:
            if method(volume, 7, HRESULT, ctypes.c_float, POINTER)(volume, percent / 100, None) < 0:
                raise OSError("Windows could not change the audio level.")

    def set_muted(self, capture, muted):
        with self._default_volume(capture) as volume:
            if method(volume, 14, HRESULT, ctypes.c_int, POINTER)(volume, int(muted), None) < 0:
                raise OSError("Windows could not change mute.")

    def close(self):
        for meter in self.meters or ():
            release(meter)
        release(self.enumerator)
        self.meters = None
        self.enumerator = POINTER()
        if self.initialized:
            self.ole.CoUninitialize()
            self.initialized = False

<#
.SYNOPSIS
    Controls Apple Music on Windows over UDP, for AirPlay senders librespot cannot reach by DACP.

.DESCRIPTION
    When Apple Music on Windows streams to this fork's AirPlay receiver, the receiver cannot send
    transport commands back. It does get the sender's DACP-ID/Active-Remote headers, but the
    matching iTunes_Ctrl_<id>._dacp._tcp advertisement never resolves over mDNS, so
    dacp::resolve_port times out and next/pause/resume have nowhere to go.

    This script fills that gap from the other end: it listens on a UDP port and drives the player
    locally, through System Media Transport Controls (transport) and IAudioEndpointVolume
    (volume). It needs no mDNS, no DACP and nothing from librespot.

    Clients never talk to this script directly. They keep talking to the API on the Pi exactly as
    before; librespot forwards next/pause/resume here on its own, to the address the sender's RTSP
    connection came from (AirplayEvent::SessionStarted) and the port given by
    --airplay-helper-port, and only while AirPlay is playing and no DACP endpoint resolved.
    Datagrams therefore arrive from the Pi, not from a client, so it is the Pi's address that has
    to be in -Allow.

    Volume is not forwarded, because it is not broken: the sender pushes its own volume in with
    SET_PARAMETER, and setvol attenuates the receiver's output. volup/voldown/setvol are still
    handled here, since they work when sent to this script directly, which is useful for testing.

    The command vocabulary is taken verbatim from ApiServerTask::handle_request (src/server.rs).
    Query replies use the same JSON shapes as TrackResponse, VolumeResponse and Event::Snapshot.

    Every command is answered, forwarded ones included. The Pi does not want those answers and
    sends its forwards from a socket of its own precisely so they land nowhere; the replies are
    here for getvol/current_track/status, which are useless without one, and for testing by hand.
    Do not drop them to save the Pi the trouble - it is not reading them.

.PARAMETER Port
    UDP port to listen on. Must match --airplay-helper-port on the Pi (50506 by default).

.PARAMETER Allow
    CIDR networks allowed to send commands, like --api-allow. Must cover the Pi's address, since
    the Pi is what forwards. Loopback is always allowed; an empty list allows everyone.

.PARAMETER AppPattern
    Regex matching the SMTC session by SourceAppUserModelId, so commands reach the player that is
    streaming and nothing else. When no session matches, no command is sent at all - anything else
    would mean pausing whatever happens to be playing, a browser playing YouTube included.

    Passing an empty string opts out of that protection and controls the system's current session,
    whatever it is. Only sensible on a machine that plays nothing but this one app.

.EXAMPLE
    powershell.exe -ExecutionPolicy Bypass -File .\airplay-control-windows.ps1

.EXAMPLE
    # Nothing has to be sent by hand in normal use. To check the script itself, from the Pi:
    #   echo -n 'pause'                    | nc -u -w1 172.30.2.8 50506
    #   echo -n 'setvol {"volume": 32768}' | nc -u -w1 172.30.2.8 50506

.NOTES
    Run this with Windows PowerShell 5.1 (powershell.exe), NOT PowerShell 7 (pwsh): the WinRT
    projections SMTC relies on are unavailable in 7 without extra packages.

    This file is deliberately pure ASCII. Windows PowerShell 5.1 decodes .ps1 files using the
    system ANSI code page unless they carry a UTF-8 BOM, so any non-ASCII character here would
    arrive mangled and can break parsing outright.
#>

[CmdletBinding()]
param(
    [int]$Port = 50506,
    [string[]]$Allow = @('172.30.2.0/24'),
    [string]$AppPattern = 'AppleMusic|iTunes'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# --- allow list (mirrors AllowList in src/server.rs) ----------------------------------------
# Compared byte by byte rather than by packing the address into an integer and masking it. The
# integer form needs a 32-bit mask built with a shift, and PowerShell's shift and bitwise
# operators pick their own result types, which is easy to get wrong and hard to see. Bytes need
# no packing, no width and no sign, and this way IPv6 works for free.
function Test-PrefixMatch {
    param([byte[]]$Left, [byte[]]$Right, [int]$Bits)

    $wholeBytes = [int][math]::Floor($Bits / 8)
    for ($i = 0; $i -lt $wholeBytes; $i++) {
        if ($Left[$i] -ne $Right[$i]) { return $false }
    }

    $remaining = $Bits % 8
    if ($remaining -eq 0) { return $true }

    # The top `remaining` bits of the byte the prefix ends inside of.
    $mask = [byte]((0xFF -shl (8 - $remaining)) -band 0xFF)

    return ($Left[$wholeBytes] -band $mask) -eq ($Right[$wholeBytes] -band $mask)
}

function Test-Allowed {
    param([System.Net.IPAddress]$Address, [string[]]$Cidrs)

    if ([System.Net.IPAddress]::IsLoopback($Address)) { return $true }
    if ($null -eq $Cidrs -or $Cidrs.Count -eq 0) { return $true }

    $addrBytes = $Address.GetAddressBytes()

    foreach ($cidr in $Cidrs) {
        $parts = $cidr.Split('/')

        $baseIp = $null
        if (-not [System.Net.IPAddress]::TryParse($parts[0], [ref]$baseIp)) { continue }
        $baseBytes = $baseIp.GetAddressBytes()

        # An IPv4 rule never matches an IPv6 peer, or the other way round.
        if ($baseBytes.Length -ne $addrBytes.Length) { continue }

        $width = $baseBytes.Length * 8
        $bits = $width
        if ($parts.Count -gt 1) {
            $parsed = 0
            if (-not [int]::TryParse($parts[1], [ref]$parsed)) { continue }
            $bits = $parsed
        }
        if ($bits -lt 0 -or $bits -gt $width) { continue }

        if (Test-PrefixMatch $addrBytes $baseBytes $bits) { return $true }
    }

    return $false
}

# --- system volume: IAudioEndpointVolume ----------------------------------------------------
# The API scale is u16 (0..65535, SetVolumeRequest/VolumeResponse); Windows works in 0..1, so the
# conversion lives here, on the side that knows both.
if (-not ('WinAudio' -as [type])) {
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class WinAudio {
    [ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")]
    private class MMDeviceEnumerator { }

    [Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IMMDeviceEnumerator {
        int EnumAudioEndpoints(int dataFlow, int stateMask, out IntPtr devices);
        int GetDefaultAudioEndpoint(int dataFlow, int role, out IMMDevice device);
        int GetDevice(string id, out IMMDevice device);
        int RegisterEndpointNotificationCallback(IntPtr client);
        int UnregisterEndpointNotificationCallback(IntPtr client);
    }

    [Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IMMDevice {
        int Activate(ref Guid iid, int clsCtx, IntPtr activationParams,
                     [MarshalAs(UnmanagedType.IUnknown)] out object iface);
        int OpenPropertyStore(int access, out IntPtr store);
        int GetId(out IntPtr id);
        int GetState(out int state);
    }

    [Guid("5CDF2C82-841E-4546-9722-0CF74078229A"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IAudioEndpointVolume {
        int RegisterControlChangeNotify(IntPtr notify);
        int UnregisterControlChangeNotify(IntPtr notify);
        int GetChannelCount(out uint count);
        int SetMasterVolumeLevel(float levelDb, ref Guid ctx);
        int SetMasterVolumeLevelScalar(float level, ref Guid ctx);
        int GetMasterVolumeLevel(out float levelDb);
        int GetMasterVolumeLevelScalar(out float level);
        int SetChannelVolumeLevel(uint channel, float levelDb, ref Guid ctx);
        int SetChannelVolumeLevelScalar(uint channel, float level, ref Guid ctx);
        int GetChannelVolumeLevel(uint channel, out float levelDb);
        int GetChannelVolumeLevelScalar(uint channel, out float level);
        int SetMute([MarshalAs(UnmanagedType.Bool)] bool mute, ref Guid ctx);
        int GetMute([MarshalAs(UnmanagedType.Bool)] out bool mute);
        int GetVolumeStepInfo(out uint step, out uint stepCount);
        int VolumeStepUp(ref Guid ctx);
        int VolumeStepDown(ref Guid ctx);
        int QueryHardwareSupport(out uint mask);
        int GetVolumeRange(out float min, out float max, out float increment);
    }

    private static IAudioEndpointVolume Endpoint() {
        var enumerator = (IMMDeviceEnumerator)(new MMDeviceEnumerator());
        IMMDevice device;
        // eRender, eMultimedia
        Marshal.ThrowExceptionForHR(enumerator.GetDefaultAudioEndpoint(0, 1, out device));
        var iid = typeof(IAudioEndpointVolume).GUID;
        object iface;
        // CLSCTX_ALL
        Marshal.ThrowExceptionForHR(device.Activate(ref iid, 23, IntPtr.Zero, out iface));
        return (IAudioEndpointVolume)iface;
    }

    /// Current volume as 0..1.
    public static float Get() {
        float level;
        Marshal.ThrowExceptionForHR(Endpoint().GetMasterVolumeLevelScalar(out level));
        return level;
    }

    public static void Set(float level) {
        if (level < 0f) { level = 0f; }
        if (level > 1f) { level = 1f; }
        var ctx = Guid.Empty;
        Marshal.ThrowExceptionForHR(Endpoint().SetMasterVolumeLevelScalar(level, ref ctx));
    }

    /// One step of whatever Windows considers a step, same as a volume key.
    public static void StepUp() {
        var ctx = Guid.Empty;
        Marshal.ThrowExceptionForHR(Endpoint().VolumeStepUp(ref ctx));
    }

    public static void StepDown() {
        var ctx = Guid.Empty;
        Marshal.ThrowExceptionForHR(Endpoint().VolumeStepDown(ref ctx));
    }
}
'@
}

# --- SMTC: transport and metadata -----------------------------------------------------------
$script:SmtcReady = $false
$script:AsTaskGeneric = $null

try {
    Add-Type -AssemblyName System.Runtime.WindowsRuntime
    # WinRT hands back IAsyncOperation<T>, which PowerShell cannot await on its own; this is the
    # usual System.Runtime.WindowsRuntime bridge to a Task<T>.
    $script:AsTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() |
        Where-Object {
            $_.Name -eq 'AsTask' -and
            $_.GetParameters().Count -eq 1 -and
            $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
        })[0]
    [void][Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media.Control, ContentType = WindowsRuntime]
    $script:SmtcReady = $true
} catch {
    Write-Warning "SMTC is unavailable: $($_.Exception.Message)"
    Write-Warning "If this is pwsh (PowerShell 7), run powershell.exe (5.1) instead. Transport will not work."
}

function Await {
    param($Operation, [Type]$ResultType)
    $asTask = $script:AsTaskGeneric.MakeGenericMethod($ResultType)
    $task = $asTask.Invoke($null, @($Operation))
    [void]$task.Wait(5000)
    return $task.Result
}

function Get-MusicSession {
    if (-not $script:SmtcReady) { return $null }
    try {
        $manager = Await ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]::RequestAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager])
        if ($null -eq $manager) { return $null }

        # An empty pattern is an explicit "control whatever is playing", and only then is the
        # system's current session used.
        if ([string]::IsNullOrWhiteSpace($AppPattern)) {
            return $manager.GetCurrentSession()
        }

        $seen = @()
        foreach ($session in $manager.GetSessions()) {
            $id = $session.SourceAppUserModelId
            $seen += $id
            if ($id -match $AppPattern) { return $session }
        }

        # Nothing matched, and this deliberately does NOT fall back to the current session. That
        # fallback used to be here and is exactly how a pause meant for Apple Music could stop a
        # YouTube tab instead: on a machine where something else is playing, the current session
        # is that something else. Doing nothing is the only safe answer, so say why and stop.
        Write-Host (
            "no session matching /$AppPattern/; refusing to control another app. Sessions: " +
            $(if ($seen.Count) { $seen -join ', ' } else { '(none)' })
        ) -ForegroundColor Yellow

        return $null
    } catch {
        return $null
    }
}

function Invoke-Transport {
    param([ValidateSet('next', 'pause', 'resume')][string]$Command)

    $session = Get-MusicSession
    if ($null -eq $session) { return $false }
    try {
        switch ($Command) {
            'next'   { [void](Await ($session.TrySkipNextAsync()) ([bool])); return $true }
            'pause'  { [void](Await ($session.TryPauseAsync())    ([bool])); return $true }
            'resume' { [void](Await ($session.TryPlayAsync())     ([bool])); return $true }
        }
    } catch {
        return $false
    }
    return $false
}

# Rounded rather than truncated, so 100% lands exactly on 65535 - the same rule as
# volume_from_percent in src/server.rs.
function ConvertTo-ApiVolume { param([float]$Scalar) return [uint16][math]::Round($Scalar * 65535.0) }
function ConvertTo-WinVolume { param([int]$Volume)  return [float]($Volume / 65535.0) }

# The TrackResponse shape: every field always present, empty rather than omitted, because this
# fork's clients rely on that. `source` is "airplay", which is what this path is.
function Get-TrackResponse {
    $track = [ordered]@{
        song_name = ''; song_id = ''; song_artists = @(); song_uri = ''
        item_type = 'airplay'; album = ''; album_artists = @()
        duration_ms = 0; is_explicit = $false; source = 'airplay'
    }

    $session = Get-MusicSession
    if ($null -eq $session) { return $track }
    try {
        $props = Await ($session.TryGetMediaPropertiesAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties])
        $timeline = $session.GetTimelineProperties()

        $track.song_name = [string]$props.Title
        if (-not [string]::IsNullOrEmpty($props.Artist)) {
            $track.song_artists = @([string]$props.Artist)
        }
        if (-not [string]::IsNullOrEmpty($props.AlbumArtist)) {
            $track.album_artists = @([string]$props.AlbumArtist)
        }
        $track.album = [string]$props.AlbumTitle
        $track.duration_ms = [uint32][math]::Max([double]0, $timeline.EndTime.TotalMilliseconds)
    } catch {
        # Leave the empty shape rather than a half-filled one.
    }
    return $track
}

function Get-Snapshot {
    $isPlaying = $false
    $positionMs = 0

    $session = Get-MusicSession
    if ($null -ne $session) {
        try {
            # Compared as a string so this does not depend on the enum type being projected.
            $isPlaying = ("$($session.GetPlaybackInfo().PlaybackStatus)" -eq 'Playing')
            $positionMs = [uint32][math]::Max([double]0, $session.GetTimelineProperties().Position.TotalMilliseconds)
        } catch {
            # Same as above: report what is known rather than failing the whole query.
        }
    }

    return [ordered]@{
        event       = 'snapshot'
        is_playing  = $isPlaying
        position_ms = $positionMs
        volume      = ConvertTo-ApiVolume ([WinAudio]::Get())
        track       = Get-TrackResponse
    }
}

# --- command dispatch -----------------------------------------------------------------------
# The vocabulary of ApiServerTask::handle_request, minus what belongs to the Pi. Every arm assigns
# $reply and the function returns it explicitly, so nothing an arm happens to emit leaks into the
# result.
function Invoke-ApiCommand {
    param([string]$Command, [string]$Payload)

    $reply = "unknown command '$Command'"

    switch ($Command) {
        { $_ -in 'next', 'pause', 'resume' } {
            $reply = if (Invoke-Transport $_) { 'ok' } else { 'no session' }
        }

        'volup'   { [WinAudio]::StepUp();   $reply = 'ok' }
        'voldown' { [WinAudio]::StepDown(); $reply = 'ok' }

        'setvol' {
            try {
                $request = $Payload | ConvertFrom-Json
                [WinAudio]::Set((ConvertTo-WinVolume ([int]$request.volume)))
                $reply = 'ok'
            } catch {
                Write-Host "invalid setvol payload '$Payload'" -ForegroundColor Yellow
                $reply = 'invalid payload'
            }
        }

        'getvol' {
            $reply = ([ordered]@{ volume = ConvertTo-ApiVolume ([WinAudio]::Get()) } |
                ConvertTo-Json -Compress)
        }

        'current_track' { $reply = Get-TrackResponse | ConvertTo-Json -Compress -Depth 4 }
        'status'        { $reply = Get-Snapshot      | ConvertTo-Json -Compress -Depth 4 }

        # cover, subscribe and unsubscribe are deliberately not handled here: AirPlay metadata and
        # artwork reach the Pi over the sender's own push path (SET_PARAMETER), which works. Only
        # control is broken, and only control is replaced - a second event server on Windows would
        # duplicate a working one. Clients ask the Pi for these.
        { $_ -in 'cover', 'subscribe', 'unsubscribe' } {
            $reply = 'not handled here: ask the Pi'
        }
    }

    return $reply
}

# --- loop -----------------------------------------------------------------------------------
$client = [System.Net.Sockets.UdpClient]::new(
    [System.Net.IPEndPoint]::new([System.Net.IPAddress]::Any, $Port))

# Without this, Receive() blocks forever and Ctrl+C does nothing: PowerShell only notices a stop
# request between statements, never inside a blocking .NET call. Returning every second gives it
# that chance. api_test.py does the same thing with sock.settimeout(1).
$client.Client.ReceiveTimeout = 1000

$smtcState = if ($script:SmtcReady) { 'yes' } else { 'NO - transport will not work' }

Write-Host "Listening on UDP 0.0.0.0:$Port" -ForegroundColor Green
Write-Host "Allowed: $($Allow -join ', ') (plus loopback)" -ForegroundColor Green
Write-Host "SMTC: $smtcState" -ForegroundColor Green
Write-Host "Handled: next pause resume volup voldown setvol getvol current_track status" -ForegroundColor DarkGray
Write-Host "Ctrl+C to quit." -ForegroundColor DarkGray

try {
    while ($true) {
        $peer = [System.Net.IPEndPoint]::new([System.Net.IPAddress]::Any, 0)

        try {
            $datagram = $client.Receive([ref]$peer)
        } catch [System.Net.Sockets.SocketException] {
            if ($_.Exception.SocketErrorCode -ne [System.Net.Sockets.SocketError]::TimedOut) {
                Write-Host "receive failed: $($_.Exception.Message)" -ForegroundColor Yellow
            }
            # A timeout is the normal case: it is what makes Ctrl+C possible at all.
            continue
        }

        # Parsed the way handle_request parses it: "<command>[ <json payload>]". The -split
        # operator takes its own limit, so there is no .NET overload to pick wrong.
        $message = [System.Text.Encoding]::UTF8.GetString($datagram).Trim()
        if ($message.Length -eq 0) { continue }

        $pieces = $message -split '\s+', 2
        $command = $pieces[0].ToLowerInvariant()
        $payload = if ($pieces.Count -gt 1) { $pieces[1].Trim() } else { '' }

        if (-not (Test-Allowed -Address $peer.Address -Cidrs $Allow)) {
            Write-Host "rejected '$command' from $($peer.Address): not on the allow list" -ForegroundColor Yellow
            continue
        }

        # One bad command must not take the listener down with it, and an unexpected failure
        # should say where it happened rather than just what it was.
        try {
            $reply = Invoke-ApiCommand -Command $command -Payload $payload
        } catch {
            Write-Host (
                "'$command' failed at line $($_.InvocationInfo.ScriptLineNumber): " +
                "$($_.Exception.GetType().Name): $($_.Exception.Message)"
            ) -ForegroundColor Red
            $reply = 'error'
        }

        Write-Host "$($peer.Address) -> '$command' => $reply"
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($reply)
        [void]$client.Send($bytes, $bytes.Length, $peer)
    }
} finally {
    $client.Close()
    Write-Host 'socket closed' -ForegroundColor DarkGray
}

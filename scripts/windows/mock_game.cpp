// SPDX-License-Identifier: MIT OR Apache-2.0
// Standalone capture fixture. No Clipline linkage, injection, or capture APIs.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <mmsystem.h>
#include <d3d11_1.h>
#include <dxgi1_2.h>
#include <wrl/client.h>
#include <algorithm>
#include <array>
#include <atomic>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <stdexcept>
#include <string>
#include <thread>

using Microsoft::WRL::ComPtr;

static void check(HRESULT hr, const char* operation) {
    if (FAILED(hr)) {
        char text[160];
        sprintf_s(text, "%s failed: 0x%08lX", operation, static_cast<unsigned long>(hr));
        throw std::runtime_error(text);
    }
}

class Audio {
    // Four 100 ms buffers tolerate Windows timer/scheduler delays. Four 10 ms
    // buffers reproduced underruns on the Windows 10 virtual-output host.
    static constexpr unsigned rate = 48000, frames = 4800;
    HWAVEOUT output = nullptr;
    std::array<std::array<short, frames * 2>, 4> data{};
    std::array<WAVEHDR, 4> headers{};
    std::uint64_t submitted = 0;
    std::atomic<bool> running{false};
    std::atomic<MMRESULT> failure{MMSYSERR_NOERROR};
    std::thread worker;
public:
    Audio() = default;
    Audio(const Audio&) = delete;
    Audio& operator=(const Audio&) = delete;
    ~Audio() {
        running = false;
        if (worker.joinable()) worker.join();
        if (!output) return;
        waveOutReset(output);
        for (auto& h : headers) if (h.dwFlags & WHDR_PREPARED)
            waveOutUnprepareHeader(output, &h, sizeof(h));
        waveOutClose(output);
    }
    void start() {
        WAVEFORMATEX format{};
        format.wFormatTag = WAVE_FORMAT_PCM;
        format.nChannels = 2;
        format.nSamplesPerSec = rate;
        format.wBitsPerSample = 16;
        format.nBlockAlign = 4;
        format.nAvgBytesPerSec = rate * 4;
        const auto result = waveOutOpen(&output, WAVE_MAPPER, &format, 0, 0, CALLBACK_NULL);
        if (result != MMSYSERR_NOERROR) throw std::runtime_error("No usable default stereo audio endpoint");
        for (size_t i = 0; i < headers.size(); ++i) {
            headers[i].lpData = reinterpret_cast<LPSTR>(data[i].data());
            headers[i].dwBufferLength = static_cast<DWORD>(sizeof(data[i]));
            if (waveOutPrepareHeader(output, &headers[i], sizeof(WAVEHDR)) != MMSYSERR_NOERROR)
                throw std::runtime_error("waveOutPrepareHeader failed");
            if (queue(i) != MMSYSERR_NOERROR) throw std::runtime_error("waveOutWrite failed");
        }
        running = true;
        worker = std::thread([this] {
            while (running) {
                for (size_t i = 0; i < headers.size(); ++i) {
                    if (headers[i].dwFlags & WHDR_DONE) {
                        const auto result = queue(i);
                        if (result != MMSYSERR_NOERROR) { failure = result; return; }
                    }
                }
                Sleep(2);
            }
        });
    }
    MMRESULT queue(size_t i) {
        for (unsigned f = 0; f < frames; ++f, ++submitted) {
            const double t = static_cast<double>(submitted) / rate;
            const double gain = (submitted / rate) % 2 == 0 ? 0.2 : 0.0;
            data[i][f * 2] = static_cast<short>(32767 * gain * std::sin(6.283185307179586 * 440 * t));
            data[i][f * 2 + 1] = static_cast<short>(32767 * gain * std::sin(6.283185307179586 * 880 * t));
        }
        return waveOutWrite(output, &headers[i], sizeof(WAVEHDR));
    }
    void pump() {
        if (failure != MMSYSERR_NOERROR) throw std::runtime_error("Audio pump failed");
    }
    double seconds() const {
        MMTIME position{};
        position.wType = TIME_SAMPLES;
        if (waveOutGetPosition(output, &position, sizeof(position)) != MMSYSERR_NOERROR)
            throw std::runtime_error("waveOutGetPosition failed");
        if (position.wType == TIME_SAMPLES) return static_cast<double>(position.u.sample) / rate;
        if (position.wType == TIME_BYTES) return static_cast<double>(position.u.cb) / (rate * 4);
        if (position.wType == TIME_MS) return static_cast<double>(position.u.ms) / 1000;
        throw std::runtime_error("Unsupported audio position units");
    }
};

struct App {
    HWND window = nullptr;
    bool resize = true, toggleBorderless = false, toggleExclusive = false;
    bool borderless = false, exclusive = false, flip = false;
    RECT savedRect{};
    ComPtr<ID3D11Device> device;
    ComPtr<ID3D11DeviceContext> context;
    ComPtr<ID3D11DeviceContext1> context1;
    ComPtr<IDXGISwapChain1> swap;
    ComPtr<ID3D11RenderTargetView> target;
    std::wstring adapterName;
    Audio audio;

    void graphics() {
        check(D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0, D3D11_SDK_VERSION,
            &device, nullptr, &context), "hardware D3D11CreateDevice");
        check(context.As(&context1), "ID3D11DeviceContext1");
        ComPtr<IDXGIDevice> dxgi;
        ComPtr<IDXGIAdapter> adapter;
        ComPtr<IDXGIFactory2> factory;
        check(device.As(&dxgi), "IDXGIDevice");
        check(dxgi->GetAdapter(&adapter), "GetAdapter");
        DXGI_ADAPTER_DESC description{};
        check(adapter->GetDesc(&description), "adapter description");
        adapterName = description.Description;
        check(adapter->GetParent(IID_PPV_ARGS(&factory)), "IDXGIFactory2");
        DXGI_SWAP_CHAIN_DESC1 desc{};
        desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        desc.SampleDesc.Count = 1;
        desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
        desc.BufferCount = flip ? 2 : 1;
        desc.SwapEffect = flip ? DXGI_SWAP_EFFECT_FLIP_DISCARD : DXGI_SWAP_EFFECT_DISCARD;
        desc.Flags = DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH;
        check(factory->CreateSwapChainForHwnd(device.Get(), window, &desc, nullptr, nullptr, &swap), "CreateSwapChainForHwnd");
        check(factory->MakeWindowAssociation(window, DXGI_MWA_NO_ALT_ENTER), "MakeWindowAssociation");
    }
    void changeMode() {
        BOOL actual = FALSE;
        check(swap->GetFullscreenState(&actual, nullptr), "GetFullscreenState");
        // DXGI may leave fullscreen on focus loss without a useful size change.
        // Flip-model buffers must be resized even when dimensions stay the same.
        if (exclusive != (actual != FALSE)) resize = true;
        exclusive = actual != FALSE;
        if (toggleExclusive) {
            toggleExclusive = false;
            const bool next = !exclusive;
            const HRESULT result = swap->SetFullscreenState(next, nullptr);
            check(result, "SetFullscreenState");
            if (result != S_OK) throw std::runtime_error("Fullscreen transition did not complete");
            check(swap->GetFullscreenState(&actual, nullptr), "confirm fullscreen");
            exclusive = actual != FALSE;
            if (exclusive != next) throw std::runtime_error("Requested fullscreen state was not reached");
            resize = true;
        }
        if (toggleBorderless) {
            toggleBorderless = false;
            if (exclusive) {
                const auto result = swap->SetFullscreenState(FALSE, nullptr);
                check(result, "exit exclusive");
                if (result != S_OK) throw std::runtime_error("Exit fullscreen did not complete");
                exclusive = false;
            }
            if (!borderless) {
                GetWindowRect(window, &savedRect);
                MONITORINFO monitor{sizeof(MONITORINFO)};
                if (!GetMonitorInfoW(MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST), &monitor))
                    throw std::runtime_error("GetMonitorInfo failed");
                SetWindowLongPtrW(window, GWL_STYLE, WS_POPUP | WS_VISIBLE);
                SetWindowPos(window, HWND_TOP, monitor.rcMonitor.left, monitor.rcMonitor.top,
                    monitor.rcMonitor.right - monitor.rcMonitor.left, monitor.rcMonitor.bottom - monitor.rcMonitor.top,
                    SWP_FRAMECHANGED);
            } else {
                SetWindowLongPtrW(window, GWL_STYLE, WS_OVERLAPPEDWINDOW | WS_VISIBLE);
                SetWindowPos(window, nullptr, savedRect.left, savedRect.top,
                    savedRect.right - savedRect.left, savedRect.bottom - savedRect.top,
                    SWP_FRAMECHANGED | SWP_NOZORDER);
            }
            borderless = !borderless;
            resize = true;
        }
    }
    void draw(std::uint64_t frame) {
        if (resize) {
            context->OMSetRenderTargets(0, nullptr, nullptr);
            target.Reset();
            check(swap->ResizeBuffers(0, 0, 0, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH), "ResizeBuffers");
            ComPtr<ID3D11Texture2D> buffer;
            check(swap->GetBuffer(0, IID_PPV_ARGS(&buffer)), "GetBuffer");
            check(device->CreateRenderTargetView(buffer.Get(), nullptr, &target), "CreateRenderTargetView");
            resize = false;
        }
        RECT client{};
        GetClientRect(window, &client);
        const double time = audio.seconds();
        const bool tone = static_cast<unsigned>(time) % 2 == 0;
        const float navy[]{0, 0, 0.20f, 1}, red[]{1, 0, 0, 1}, blue[]{0, 0, 1, 1};
        const float green[]{0, 1, 0, 1}, yellow[]{1, 1, 0, 1}, white[]{1, 1, 1, 1};
        context->ClearRenderTargetView(target.Get(), navy);
        const D3D11_RECT r{0, 0, 80, 80}, b{80, 0, 160, 80};
        context1->ClearView(target.Get(), red, &r, 1);
        context1->ClearView(target.Get(), blue, &b, 1);
        const LONG x = static_cast<LONG>(std::fmod(time * 180, static_cast<double>(std::max(1L, client.right - 100))));
        const D3D11_RECT moving{x, 140, x + 90, 230};
        context1->ClearView(target.Get(), green, &moving, 1);
        const D3D11_RECT pulse{200, 10, 280, 90};
        context1->ClearView(target.Get(), tone ? yellow : blue, &pulse, 1);
        // Binary frame counter remains visible in borderless/exclusive mode.
        for (unsigned bit = 0; bit < 16; ++bit) {
            const LONG left = 10 + static_cast<LONG>(bit) * 22;
            const D3D11_RECT cell{left, 100, left + 18, 120};
            context1->ClearView(target.Get(), frame & (1ull << bit) ? white : blue, &cell, 1);
        }
        if (frame % 15 == 0) {
            wchar_t title[512];
            swprintf_s(title, L"Clipline Mock %s | %s | %s | frame %llu | audio %.2fs | F11 borderless, F10 exclusive, Esc quit",
                flip ? L"Flip" : L"Blt", adapterName.c_str(), exclusive ? L"exclusive" : borderless ? L"borderless" : L"windowed",
                static_cast<unsigned long long>(frame), time);
            SetWindowTextW(window, title);
        }
        const HRESULT result = swap->Present(1, 0);
        if (result == DXGI_STATUS_OCCLUDED) Sleep(10);
        else if (result == DXGI_ERROR_INVALID_CALL) {
            BOOL actual = FALSE;
            check(swap->GetFullscreenState(&actual, nullptr), "fullscreen after Present");
            // A focus transition can race changeMode/draw. Retry only a confirmed
            // mode change or resize notification; unrelated errors remain fatal.
            if (exclusive == (actual != FALSE) && !resize) check(result, "Present");
            exclusive = actual != FALSE;
            resize = true;
        }
        else check(result, "Present");
    }
    ~App() {
        if (swap) swap->SetFullscreenState(FALSE, nullptr);
        // Error dialogs pump messages too: never leave userdata pointing to an
        // App that has already unwound after a graphics/audio failure.
        if (window && IsWindow(window)) DestroyWindow(window);
    }
};

static LRESULT CALLBACK windowProc(HWND window, UINT message, WPARAM key, LPARAM value) {
    auto* app = reinterpret_cast<App*>(GetWindowLongPtrW(window, GWLP_USERDATA));
    if (message == WM_NCDESTROY) {
        SetWindowLongPtrW(window, GWLP_USERDATA, 0);
        return DefWindowProcW(window, message, key, value);
    }
    if (message == WM_NCCREATE) {
        app = static_cast<App*>(reinterpret_cast<CREATESTRUCTW*>(value)->lpCreateParams);
        SetWindowLongPtrW(window, GWLP_USERDATA, reinterpret_cast<LONG_PTR>(app));
    }
    if (message == WM_DESTROY) { PostQuitMessage(0); return 0; }
    if (app && message == WM_SIZE && key != SIZE_MINIMIZED) app->resize = true;
    if (app && message == WM_KEYDOWN && !(value & (1LL << 30))) {
        if (key == VK_F11) app->toggleBorderless = true;
        if (key == VK_F10) app->toggleExclusive = true;
        if (key == VK_ESCAPE) DestroyWindow(window);
        return 0;
    }
    return DefWindowProcW(window, message, key, value);
}

int WINAPI wWinMain(HINSTANCE instance, HINSTANCE, PWSTR command, int) {
    try {
        SetProcessDPIAware();
        App app;
        wchar_t executable[MAX_PATH]{};
        GetModuleFileNameW(nullptr, executable, MAX_PATH);
        const auto* separator = wcsrchr(executable, L'\\');
        app.flip = wcsstr(separator ? separator + 1 : executable, L"Flip") != nullptr;
        if (wcsstr(command, L"--flip")) app.flip = true;
        if (wcsstr(command, L"--blt")) app.flip = false;
        app.toggleBorderless = wcsstr(command, L"--borderless") != nullptr;
        app.toggleExclusive = wcsstr(command, L"--exclusive") != nullptr;
        // Reproducible physical capture transition without external input tools.
        const bool canvasCycle = wcsstr(command, L"--canvas-cycle") != nullptr;
        unsigned duration = 180;
        if (const auto* option = wcsstr(command, L"--seconds ")) {
            wchar_t* end = nullptr;
            const auto parsed = wcstoul(option + 10, &end, 10);
            if (end == option + 10 || parsed < 1 || parsed > 600)
                throw std::runtime_error("--seconds must be 1..600");
            duration = static_cast<unsigned>(parsed);
        }
        WNDCLASSW wc{};
        wc.lpfnWndProc = windowProc;
        wc.hInstance = instance;
        wc.lpszClassName = L"CliplineMockGame";
        wc.hCursor = LoadCursorW(nullptr, IDC_ARROW);
        if (!RegisterClassW(&wc)) throw std::runtime_error("RegisterClass failed");
        app.window = CreateWindowExW(0, wc.lpszClassName, app.flip ? L"Clipline Mock Flip" : L"Clipline Mock Blt",
            WS_OVERLAPPEDWINDOW, 180, 100, 816, 489, nullptr, nullptr, instance, &app);
        if (!app.window) throw std::runtime_error("CreateWindow failed");
        app.graphics();
        ShowWindow(app.window, SW_SHOWNORMAL);
        // The opt-in fullscreen fixture requires foreground ownership. Request
        // normal activation once; Windows may deny it, which the stage log shows.
        if (canvasCycle) SetForegroundWindow(app.window);
        app.audio.start();
        const ULONGLONG started = GetTickCount64();
        bool running = true;
        std::uint64_t frame = 0;
        unsigned cycleStage = 0;
        while (running && GetTickCount64() - started < static_cast<ULONGLONG>(duration) * 1000) {
            MSG message{};
            while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
                if (message.message == WM_QUIT) { running = false; break; }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            if (!running) break;
            app.audio.pump();
            const auto elapsed = GetTickCount64() - started;
            bool logStage = canvasCycle && frame == 0;
            if (canvasCycle && ((cycleStage == 0 && elapsed >= 8000) ||
                (cycleStage == 1 && elapsed >= 16000))) {
                app.toggleExclusive = true;
                ++cycleStage;
                logStage = true;
            } else if (canvasCycle && cycleStage == 2 && elapsed >= 20000) {
                app.toggleBorderless = true;
                ++cycleStage;
                logStage = true;
            }
            app.changeMode();
            if (logStage && cycleStage != 0 && app.exclusive != (cycleStage == 1))
                throw std::runtime_error("Canvas cycle reached unexpected fullscreen state");
            if (IsIconic(app.window)) Sleep(5);
            else app.draw(frame++);
            if (logStage) {
                RECT size{};
                GetClientRect(app.window, &size);
                fprintf(stdout, "canvas_cycle ms=%llu stage=%u client=%ldx%ld fullscreen=%d foreground=%d\n",
                    elapsed, cycleStage, size.right, size.bottom, app.exclusive ? 1 : 0,
                    GetForegroundWindow() == app.window ? 1 : 0);
                fflush(stdout);
            }
        }
        if (IsWindow(app.window)) DestroyWindow(app.window);
        return 0;
    } catch (const std::exception& error) {
        fprintf(stderr, "Mock game: %s\n", error.what());
        MessageBoxA(nullptr, error.what(), "Clipline mock game failed", MB_OK | MB_ICONERROR);
        return 1;
    }
}

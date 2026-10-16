// Experimental output-only AudioQueue sink. No GUI, microphone or global routing changes.
// Build: clang++ -std=c++20 -O2 -fblocks -Wall -Wextra -Werror this.cpp \
//   -framework AudioToolbox -framework CoreAudio -framework CoreFoundation -o sink
#include <AudioToolbox/AudioToolbox.h>
#include <CoreAudio/CoreAudio.h>
#include <dispatch/dispatch.h>
#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstring>
#include <fcntl.h>
#include <fstream>
#include <filesystem>
#include <iomanip>
#include <iostream>
#include <memory>
#include <poll.h>
#include <sstream>
#include <stdexcept>
#include <string>
#include <unistd.h>
#include <vector>

constexpr uint64_t Rate = 48000, Channels = 2, Quantum = 960;
constexpr uint64_t Segment = 96001, Capacity = 12 * Rate, MaxFrames = 65 * Rate;
using Clock = std::chrono::steady_clock;
double seconds(Clock::time_point start) { return std::chrono::duration<double>(Clock::now() - start).count(); }
void require(bool ok, const char* message) { if (!ok) throw std::runtime_error(message); }
void checked(OSStatus status, const char* operation) {
    if (status) throw std::runtime_error(std::string(operation) + " failed (" + std::to_string(status) + ")");
}
std::string quote(const std::string& text) {
    std::ostringstream out;
    out << '"';
    for (unsigned char c : text) {
        if (c == '"' || c == '\\') out << '\\' << c;
        else if (c < 32) out << "\\u" << std::hex << std::setw(4) << std::setfill('0') << int(c);
        else out << c;
    }
    out << '"';
    return out.str();
}
std::string stringValue(CFStringRef value) {
    std::array<char, 4096> bytes{};
    require(value && CFStringGetCString(value, bytes.data(), bytes.size(), kCFStringEncodingUTF8), "invalid device name/UID");
    return bytes.data();
}
template<class T> T property(AudioObjectID device, AudioObjectPropertySelector selector,
                            AudioObjectPropertyScope scope = kAudioObjectPropertyScopeGlobal) {
    AudioObjectPropertyAddress address{selector, scope, kAudioObjectPropertyElementMain};
    T value{}; UInt32 size = sizeof(value);
    checked(AudioObjectGetPropertyData(device, &address, 0, nullptr, &size, &value), "device property");
    return value;
}
std::string deviceString(AudioDeviceID device, AudioObjectPropertySelector selector) {
    auto value = property<CFStringRef>(device, selector);
    auto text = stringValue(value);
    CFRelease(value);
    return text;
}
struct Device { AudioDeviceID id; std::string uid, name; double rate; UInt32 transport; };
std::vector<Device> devices() {
    AudioObjectPropertyAddress address{kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    checked(AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &address, 0, nullptr, &size), "list size");
    std::vector<AudioDeviceID> ids(size / sizeof(AudioDeviceID));
    checked(AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, nullptr, &size, ids.data()), "list devices");
    std::vector<Device> result;
    for (auto id : ids) {
        AudioObjectPropertyAddress config{kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
        UInt32 bytes = 0;
        if (AudioObjectGetPropertyDataSize(id, &config, 0, nullptr, &bytes) || bytes < sizeof(AudioBufferList)) continue;
        std::vector<uint8_t> storage(bytes);
        auto* buffers = reinterpret_cast<AudioBufferList*>(storage.data());
        if (AudioObjectGetPropertyData(id, &config, 0, nullptr, &bytes, buffers)) continue;
        UInt32 channels = 0;
        for (UInt32 i = 0; i < buffers->mNumberBuffers; ++i) channels += buffers->mBuffers[i].mNumberChannels;
        if (channels < 2 || !property<UInt32>(id, kAudioDevicePropertyDeviceIsAlive)) continue;
        result.push_back({id, deviceString(id, kAudioDevicePropertyDeviceUID), deviceString(id, kAudioObjectPropertyName),
                          property<Float64>(id, kAudioDevicePropertyNominalSampleRate), property<UInt32>(id, kAudioDevicePropertyTransportType)});
    }
    return result;
}
std::string deviceJSON(const Device& device) {
    return "{\"uid\":" + quote(device.uid) + ",\"name\":" + quote(device.name) +
        ",\"nominal_sample_rate\":" + std::to_string(device.rate) +
        ",\"bluetooth\":" + (device.transport == kAudioDeviceTransportTypeBluetooth ||
        device.transport == kAudioDeviceTransportTypeBluetoothLE ? "true" : "false") + "}";
}

struct Underrun { uint64_t mediaFrame, silenceFrames; };
struct PCM {
    uint64_t total, rate, quantum, segment;
    std::vector<int32_t> ring = std::vector<int32_t>(Capacity * Channels);
    std::vector<int32_t> capture = std::vector<int32_t>(MaxFrames * Channels);
    std::array<Underrun, 4096> underruns{};
    std::atomic<uint64_t> written{0}, consumed{0}, rendered{0};
    std::atomic<int> error{0};
    uint64_t silence = 0, underrunCount = 0, callbacks = 0;
    bool starving = false;
    explicit PCM(uint64_t frames, uint64_t sampleRate = Rate)
        : total(frames), rate(sampleRate), quantum(sampleRate / 50), segment(2 * sampleRate + 1) {
        require(rate == 44100 || rate == Rate, "unsupported client rate");
        require(frames > 0 && frames <= 32 * rate + 100, "invalid duration");
    }

    // Single stdin producer. Publish only after the entire packet is present.
    void append(uint64_t start, const int32_t* samples, uint64_t count) {
        auto w = written.load(std::memory_order_relaxed);
        auto r = consumed.load(std::memory_order_acquire);
        require(start == w && count > 0 && count <= segment && count <= total - w, "invalid/reordered PCM packet");
        require(count <= Capacity - (w - r), "PCM queue capacity exceeded");
        auto first = std::min(count, Capacity - w % Capacity);
        std::copy_n(samples, first * Channels, ring.data() + (w % Capacity) * Channels);
        std::copy_n(samples + first * Channels, (count - first) * Channels, ring.data());
        written.store(w + count, std::memory_order_release);
    }
    // Single serial AudioQueue callback. No allocation, locks, I/O or decoding.
    uint64_t take(int32_t* destination) {
        ++callbacks;
        if (error.load(std::memory_order_relaxed)) return 0;
        auto r = consumed.load(std::memory_order_relaxed);
        auto count = std::min(quantum, total - r);
        if (!count) return 0;
        auto out = rendered.load(std::memory_order_relaxed);
        if (out + count > MaxFrames) { error.store(1); return 0; }
        if (written.load(std::memory_order_acquire) - r < count) {
            if (!starving) {
                if (underrunCount == underruns.size()) { error.store(2); return 0; }
                underruns[underrunCount++] = {r, 0};
            }
            starving = true;
            underruns[underrunCount - 1].silenceFrames += count;
            silence += count;
            std::fill_n(destination, count * Channels, 0);
        } else {
            starving = false;
            auto first = std::min(count, Capacity - r % Capacity);
            std::copy_n(ring.data() + (r % Capacity) * Channels, first * Channels, destination);
            std::copy_n(ring.data(), (count - first) * Channels, destination + first * Channels);
            consumed.store(r + count, std::memory_order_release);
        }
        std::copy_n(destination, count * Channels, capture.data() + out * Channels);
        rendered.store(out + count, std::memory_order_release);
        return count;
    }
};

void selfTest(uint64_t rate) {
    auto state = std::make_unique<PCM>(2 * Capacity + 17, rate);
    std::array<int32_t, Quantum * Channels> buffer{};
    require(state->take(buffer.data()) == state->quantum && state->consumed == 0, "starvation advanced media");
    require(std::all_of(buffer.begin(), buffer.end(), [](auto x) { return x == 0; }), "starvation was not silence");
    auto sample = [](uint64_t frame, uint64_t ch) { return int32_t((frame * 31 + ch * 7919) % 1000000) - 500000; };
    std::vector<int32_t> packet(Segment * Channels);
    while (state->consumed < state->total) {
        auto w = state->written.load();
        if (w < state->total && w - state->consumed < 3 * rate) {
            auto count = std::min(state->segment, state->total - w);
            for (uint64_t i = 0; i < count; ++i)
                for (uint64_t ch = 0; ch < Channels; ++ch) packet[i * Channels + ch] = sample(w + i, ch);
            state->append(w, packet.data(), count);
        }
        auto before = state->consumed.load();
        auto count = state->take(buffer.data());
        for (uint64_t i = 0; i < count; ++i)
            for (uint64_t ch = 0; ch < Channels; ++ch)
                require(buffer[i * Channels + ch] == sample(before + i, ch), "ring order/tail mismatch");
    }
    require(state->silence == state->quantum && state->rendered == state->total + state->quantum && state->underrunCount == 1, "bad capture counters");
    require(state->take(buffer.data()) == 0, "EOF produced extra audio");
    bool rejected = false;
    try { state->append(0, packet.data(), 1); } catch (...) { rejected = true; }
    require(rejected, "reordered packet accepted");
    std::cout << "{\"self_test\":\"passed: starvation, ordering, wraparound, tail, capture, EOF\"}" << std::endl;
}

struct Control {
    uint64_t generation = 0, nextGeneration = 0, nextTotal = 0;
    bool interrupted = false;
};

int play(const Device& device, const std::string& output, uint64_t total, float gain, uint64_t rate, Control* control = nullptr) {
    auto owner = std::make_unique<PCM>(total, rate);
    PCM* pcm = owner.get();
    dispatch_queue_t callbacks = dispatch_queue_create("stratum.poc.output", DISPATCH_QUEUE_SERIAL);
    AudioQueueRef queue = nullptr;
    AudioQueueTimelineRef timeline = nullptr;
    AudioStreamBasicDescription format{};
    format.mSampleRate = rate; format.mFormatID = kAudioFormatLinearPCM;
    format.mFormatFlags = kLinearPCMFormatFlagIsSignedInteger | kLinearPCMFormatFlagIsPacked;
    format.mBytesPerPacket = format.mBytesPerFrame = Channels * 4;
    format.mFramesPerPacket = 1; format.mChannelsPerFrame = Channels; format.mBitsPerChannel = 32;
    std::atomic<bool> drain{false};
    auto* drainFlag = &drain;
    std::atomic<bool> resetting{false};
    auto* resetFlag = &resetting;
    auto fill = ^(AudioQueueRef aq, AudioQueueBufferRef buffer) {
        if (resetFlag->load()) return;
        auto frames = pcm->take(static_cast<int32_t*>(buffer->mAudioData));
        if (!frames) return;
        buffer->mAudioDataByteSize = UInt32(frames * Channels * 4);
        auto status = AudioQueueEnqueueBuffer(aq, buffer, 0, nullptr);
        if (status) pcm->error.store(status);
        if (pcm->consumed == pcm->total) drainFlag->store(true);
    };
    try {
        checked(AudioQueueNewOutputWithDispatchQueue(&queue, &format, 0, callbacks, fill), "create output queue");
        auto uid = CFStringCreateWithCString(nullptr, device.uid.c_str(), kCFStringEncodingUTF8);
        auto status = AudioQueueSetProperty(queue, kAudioQueueProperty_CurrentDevice, &uid, sizeof(uid));
        CFRelease(uid);
        checked(status, "select output device");
        checked(AudioQueueSetParameter(queue, kAudioQueueParam_Volume, gain), "set test gain");
        checked(AudioQueueCreateTimeline(queue, &timeline), "create queue timeline");
        std::array<AudioQueueBufferRef, 3> buffers{};
        for (auto& buffer : buffers) checked(AudioQueueAllocateBuffer(queue, pcm->quantum * Channels * 4, &buffer), "allocate output buffer");
        auto before = Clock::now(), started = before;
        bool running = false, stopping = false, eof = false;
        uint64_t discontinuities = 0, stalePackets = 0;
        double queueTime = 0, hardwareRate = 0;
        std::array<uint64_t, 3> wireHeader{};
        const size_t headerSize = (control ? 3 : 2) * sizeof(uint64_t);
        auto* header = wireHeader.data() + (control ? 1 : 0);
        std::vector<int32_t> packet(Segment * Channels);
        size_t headerBytes = 0, bodyBytes = 0;
        checked(fcntl(STDIN_FILENO, F_SETFL, fcntl(STDIN_FILENO, F_GETFL) | O_NONBLOCK) < 0 ? -1 : 0, "stdin nonblocking");
        auto genJSON = control ? ",\"generation\":" + std::to_string(control->generation) : "";
        std::cout << "{\"event\":\"opened\",\"device\":" << deviceJSON(device) << genJSON << "}" << std::endl;
        while (true) {
            require(seconds(before) < 90, "output lifetime exceeded");
            require(!pcm->error.load(), "audio callback/queue failed");
            // Incremental nonblocking stdin packets; the callback never reads a pipe.
            while (!eof && (pcm->written < total || control)) {
                bool readingHeader = headerBytes < headerSize;
                auto* destination = readingHeader ? reinterpret_cast<char*>(wireHeader.data()) + headerBytes
                                                  : reinterpret_cast<char*>(packet.data()) + bodyBytes;
                size_t wanted = readingHeader ? headerSize - headerBytes : header[1] * Channels * 4 - bodyBytes;
                ssize_t got = read(STDIN_FILENO, destination, wanted);
                if (got < 0 && (errno == EAGAIN || errno == EINTR)) break;
                require(got >= 0, "PCM pipe read failed");
                if (!got) { eof = true; break; }
                if (readingHeader) {
                    headerBytes += got;
                    if (headerBytes == headerSize) {
                        if (control && header[0] == UINT64_MAX) {
                            require(wireHeader[0] > control->generation && header[1] > 0 && header[1] <= 32 * rate + 100,
                                    "invalid seek command");
                            control->nextGeneration = wireHeader[0]; control->nextTotal = header[1];
                            control->interrupted = true;
                            break;
                        }
                        require(header[1] > 0 && header[1] <= pcm->segment, "invalid PCM packet size");
                        if (!control || wireHeader[0] == control->generation)
                            require(header[0] == pcm->written && header[1] <= total - header[0], "invalid PCM header");
                        else require(wireHeader[0] < control->generation, "PCM from future generation");
                    }
                } else {
                    bodyBytes += got;
                    if (bodyBytes == header[1] * Channels * 4) {
                        if (!control || wireHeader[0] == control->generation) pcm->append(header[0], packet.data(), header[1]);
                        else {
                            ++stalePackets;
                            std::cout << "{\"event\":\"rejected\",\"generation\":" << wireHeader[0]
                                      << ",\"active_generation\":" << control->generation << "}" << std::endl;
                        }
                        headerBytes = bodyBytes = 0;
                    }
                }
            }
            if (control && control->interrupted) break;
            require(!eof || pcm->written == total, "PCM producer ended early");
            auto startupFrames = control && control->generation > 0 ? pcm->segment : 3 * pcm->segment;
            if (!running && pcm->written >= std::min(startupFrames, total)) {
                auto aq = queue;
                for (auto buffer : buffers) dispatch_sync(callbacks, ^{ fill(aq, buffer); });
                checked(AudioQueueStart(queue, nullptr), "start output");
                started = Clock::now(); running = true;
                std::cout << "{\"event\":\"started\"" << genJSON << "}" << std::endl;
            }
            if (running) {
                AudioTimeStamp stamp{}; Boolean discontinuity = false;
                if (!AudioQueueGetCurrentTime(queue, timeline, &stamp, &discontinuity)) {
                    if (stamp.mFlags & kAudioTimeStampSampleTimeValid) queueTime = std::max(queueTime, stamp.mSampleTime);
                    if (discontinuity) ++discontinuities;
                }
                if (drain && !stopping) { checked(AudioQueueStop(queue, false), "drain output"); stopping = true; }
                UInt32 active = 0, size = sizeof(active);
                checked(AudioQueueGetProperty(queue, kAudioQueueProperty_IsRunning, &active, &size), "queue running state");
                if (stopping && !active) break;
                require(seconds(started) < 70, "output did not finish");
            } else require(seconds(before) < 25, "device startup buffer timed out");
            CFStringRef actualUID = nullptr; UInt32 size = sizeof(actualUID);
            checked(AudioQueueGetProperty(queue, kAudioQueueProperty_CurrentDevice, &actualUID, &size), "read output route");
            auto actual = stringValue(actualUID); CFRelease(actualUID);
            require(actual == device.uid && property<UInt32>(device.id, kAudioDevicePropertyDeviceIsAlive), "selected output disappeared or changed");
            size = sizeof(hardwareRate);
            checked(AudioQueueGetProperty(queue, kAudioQueueDeviceProperty_SampleRate, &hardwareRate, &size), "read output rate");
            std::cout << "{\"event\":\"status\",\"accepted\":" << pcm->written << ",\"consumed\":" << pcm->consumed
                      << ",\"rendered\":" << pcm->rendered << ",\"queue_sample_time\":" << queueTime << genJSON << "}" << std::endl;
            pollfd input{STDIN_FILENO, POLLIN, 0};
            bool readControl = pcm->written < total || control;
            poll(readControl ? &input : nullptr, readControl ? 1 : 0, 20);
        }
        bool interrupted = control && control->interrupted;
        if (interrupted) {
            // Stop(true) can return queued buffers through the callback. Quiesce
            // our serial callback first so it cannot enqueue during the reset.
            dispatch_sync(callbacks, ^{ resetFlag->store(true); });
            checked(AudioQueueStop(queue, true), "flush old output");
        }
        double elapsed = seconds(started);
        UInt32 converterError = 0, size = sizeof(converterError);
        checked(AudioQueueGetProperty(queue, kAudioQueueProperty_ConverterError, &converterError, &size), "converter status");
        checked(AudioQueueDisposeTimeline(queue, timeline), "dispose timeline"); timeline = nullptr;
        checked(AudioQueueDispose(queue, true), "dispose queue"); queue = nullptr;
        dispatch_sync(callbacks, ^{});
        require(interrupted || pcm->consumed == total, "incomplete device output");
        checked(pcm->error.load(), "output callback");
        checked(converterError, "output converter");
        std::ofstream raw(output + "/rendered.s32le", std::ios::binary);
        raw.write(reinterpret_cast<char*>(pcm->capture.data()), pcm->rendered * Channels * 4);
        raw.close(); require(bool(raw), "capture write failed");
        std::ostringstream report;
        report << "{\"device\":" << deviceJSON(device) << ",\"gain\":" << gain
               << ",\"hardware_sample_rate\":" << hardwareRate << ",\"client_sample_rate\":" << rate
               << ",\"consumed_frames\":" << pcm->consumed << ",\"rendered_frames\":" << pcm->rendered
               << ",\"silence_frames\":" << pcm->silence << ",\"callbacks\":" << pcm->callbacks
               << ",\"elapsed_seconds\":" << elapsed << ",\"queue_sample_time\":" << queueTime
               << ",\"accepted_frames\":" << pcm->written << ",\"stale_packets_rejected\":" << stalePackets
               << ",\"interrupted\":" << (interrupted ? "true" : "false") << genJSON
               << ",\"timeline_discontinuities\":" << discontinuities << ",\"underruns\":[";
        for (uint64_t i = 0; i < pcm->underrunCount; ++i) {
            if (i) report << ',';
            report << "{\"at_media_frame\":" << pcm->underruns[i].mediaFrame
                   << ",\"silence_frames\":" << pcm->underruns[i].silenceFrames << '}';
        }
        report << "]}";
        std::ofstream metadata(output + "/device.json"); metadata << report.str() << '\n';
        metadata.close(); require(bool(metadata), "device report write failed");
        std::cout << "{\"event\":" << quote(interrupted ? "flushed" : "finished") << genJSON << "}" << std::endl;
        dispatch_release(callbacks);
        return 0;
    } catch (...) {
        if (timeline && queue) AudioQueueDisposeTimeline(queue, timeline);
        if (queue) AudioQueueDispose(queue, true);
        dispatch_sync(callbacks, ^{});
        dispatch_release(callbacks);
        throw;
    }
}

int main(int argc, char** argv) {
    alarm(95); // Independent process lifetime bound, including framework/pipe stalls.
    try {
        if (argc == 2 && std::string(argv[1]) == "--self-test") { selfTest(Rate); selfTest(44100); return 0; }
        if (argc == 2 && std::string(argv[1]) == "--list") {
            auto list = devices(); std::cout << '[';
            for (size_t i = 0; i < list.size(); ++i) { if (i) std::cout << ','; std::cout << deviceJSON(list[i]); }
            std::cout << ']' << std::endl; return 0;
        }
        bool interactive = false;
        uint64_t rate = Rate;
        bool rateSeen = false;
        for (int i = 9; i < argc; ++i) {
            std::string option = argv[i];
            if (option == "--interactive") { require(!interactive, "duplicate interactive flag"); interactive = true; }
            else if (option == "--rate") {
                require(!rateSeen && i + 1 < argc, "missing/duplicate rate"); rateSeen = true;
                rate = std::stoull(argv[++i]);
                require(rate == 44100 || rate == Rate, "unsupported client rate");
            } else throw std::runtime_error("unknown output option");
        }
        require(argc >= 9 && std::string(argv[1]) == "--device" && std::string(argv[3]) == "--out" &&
                std::string(argv[5]) == "--total" && std::string(argv[7]) == "--gain", "usage: --list | --self-test | --device UID --out DIR --total FRAMES --gain 0..1 [--rate 44100|48000] [--interactive]");
        float gain = std::stof(argv[8]); require(std::isfinite(gain) && gain >= 0 && gain <= 1, "invalid gain");
        for (const auto& device : devices())
            if (device.uid == argv[2]) {
                if (!interactive) return play(device, argv[4], std::stoull(argv[6]), gain, rate);
                Control control;
                uint64_t total = std::stoull(argv[6]);
                for (unsigned count = 0; count < 8; ++count) {
                    auto path = std::filesystem::path(argv[4]) / ("generation-" + std::to_string(control.generation));
                    require(std::filesystem::create_directory(path), "generation output already exists");
                    play(device, path.string(), total, gain, rate, &control);
                    if (!control.interrupted) return 0;
                    total = control.nextTotal;
                    control = Control{control.nextGeneration, 0, 0, false};
                }
                throw std::runtime_error("seek command budget exceeded");
            }
        throw std::runtime_error("selected stereo output is unavailable");
    } catch (const std::exception& error) {
        std::cerr << "Stratum output: " << error.what() << std::endl;
        return 1;
    }
}

import Darwin
import Foundation
import MachO
import os

/// Captures and symbolicates the call stack of another thread (the main
/// thread) from a watchdog thread by suspending it and walking the
/// frame-pointer chain.
///
/// Deadlock safety: while the target thread is suspended we touch **no**
/// heap allocator — only a caller-provided buffer and Mach syscalls
/// (`thread_get_state`, `vm_read_overwrite`). If the suspended main
/// thread happened to hold the malloc lock, allocating here would
/// deadlock the watchdog. So `capture` returns raw return addresses
/// only; `symbolicate` (which allocates Strings via `dladdr`) runs
/// *after* the thread is resumed — and only on the read/export path,
/// never inside `MainThreadStallMonitor.tick()`.
enum MainThreadBacktrace {
    /// Half-open virtual-address range occupied by a loaded image.
    /// Used by the idle-stack pre-filter so the capture path can decide
    /// "any frame in the app binary?" with integer comparisons instead
    /// of per-frame `dladdr`.
    struct AppImageRange: Equatable, Sendable {
        let start: UInt
        let end: UInt

        func contains(_ address: UInt) -> Bool {
            address >= start && address < end
        }
    }

    /// Suspend `thread`, walk its frame pointers, resume it, and return
    /// the raw return addresses (innermost first). Empty on failure.
    static func capture(thread: thread_t, maxFrames: Int = 64) -> [UInt] {
        guard maxFrames > 0 else { return [] }
        let buffer = UnsafeMutablePointer<UInt>.allocate(capacity: maxFrames)
        defer { buffer.deallocate() }

        guard thread_suspend(thread) == KERN_SUCCESS else { return [] }
        let count = fillFrames(thread: thread, into: buffer, maxFrames: maxFrames)
        _ = thread_resume(thread)

        return Array(UnsafeBufferPointer(start: buffer, count: count))
    }

    /// One resolved frame: the image it belongs to and its symbol,
    /// ahead of text formatting. Kept structured (rather than the
    /// rendered `String`) so callers can reason about image/symbol
    /// without re-parsing the column layout.
    struct SymbolicatedFrame: Equatable {
        let index: Int
        let image: String
        let address: UInt
        let symbol: String
        let offset: UInt
    }

    /// Per-address `dladdr` memo. Re-rendering the same stall (viewer
    /// poll, expand toggle, second export) is free; the memo is the
    /// soft floor on symbolication frequency for addresses already seen.
    /// The hard floor is that `MainThreadStallMonitor.tick()` never
    /// calls `symbolicate` at all.
    private static let addressCache = OSAllocatedUnfairLock(
        initialState: [UInt: (image: String, symbol: String, offset: UInt)]()
    )

    /// Resolve each address to its owning image/symbol via `dladdr`.
    /// Allocates, so call only after the target thread has been resumed,
    /// and only from read/export paths — never from the stall watchdog.
    static func symbolicate(_ addresses: [UInt]) -> [SymbolicatedFrame] {
        addresses.enumerated().map { idx, addr in
            if let hit = addressCache.withLock({ $0[addr] }) {
                return SymbolicatedFrame(
                    index: idx, image: hit.image, address: addr,
                    symbol: hit.symbol, offset: hit.offset
                )
            }
            var info = Dl_info()
            let image: String
            let symbol: String
            let offset: UInt
            if dladdr(UnsafeRawPointer(bitPattern: addr), &info) != 0 {
                image = info.dli_fname
                    .flatMap { String(validatingCString: $0) }
                    .map { ($0 as NSString).lastPathComponent } ?? "???"
                symbol = info.dli_sname
                    .flatMap { String(validatingCString: $0) } ?? hex(addr)
                let symAddr = UInt(bitPattern: info.dli_saddr)
                offset = (symAddr != 0 && addr >= symAddr) ? addr - symAddr : 0
            } else {
                image = "???"
                symbol = hex(addr)
                offset = 0
            }
            addressCache.withLock { cache in
                cache[addr] = (image, symbol, offset)
                // Bound the memo so a long-running process cannot grow it
                // without limit under a flood of unique stacks.
                if cache.count > 4096 {
                    cache.removeAll(keepingCapacity: true)
                    cache[addr] = (image, symbol, offset)
                }
            }
            return SymbolicatedFrame(
                index: idx, image: image, address: addr, symbol: symbol, offset: offset
            )
        }
    }

    /// Render resolved frames in the column layout `Thread.callStackSymbols` uses.
    static func format(_ frames: [SymbolicatedFrame]) -> [String] {
        frames.map {
            formatFrame(index: $0.index, image: $0.image, address: $0.address, symbol: $0.symbol, offset: $0.offset)
        }
    }

    /// Symbolicate + format in one step for display/export callers.
    static func symbolicatedStrings(_ addresses: [UInt]) -> [String] {
        format(symbolicate(addresses))
    }

    /// True when the captured stack shows no frame inside the app image —
    /// pure system/framework frames, which is the shape of an idle event
    /// loop (or any capture that landed with no app code on it). Pure
    /// function over raw addresses plus a pre-resolved range: integer
    /// comparisons only, no `dladdr`.
    ///
    /// When `appImageRange` is `nil` (no bounds resolved — e.g. under
    /// unit tests with no app bundle), returns `false` so captures are
    /// kept rather than silently dropped.
    static func isIdleEventLoopStack(
        _ addresses: [UInt],
        appImageRange: AppImageRange?
    ) -> Bool {
        guard !addresses.isEmpty else { return false }
        guard let range = appImageRange else { return false }
        return !addresses.contains { range.contains($0) }
    }

    /// Resolve `[start, end)` for the loaded image whose last path
    /// component equals `imageName`. Walks dyld's image list and unions
    /// the slid segment virtual ranges. Returns `nil` when the image is
    /// not loaded or `imageName` is empty/`nil`.
    static func resolveAppImageRange(imageName: String?) -> AppImageRange? {
        guard let imageName, !imageName.isEmpty else { return nil }
        let count = _dyld_image_count()
        for i in 0..<count {
            guard let cName = _dyld_get_image_name(i) else { continue }
            let base = (String(cString: cName) as NSString).lastPathComponent
            guard base == imageName else { continue }
            guard let header = _dyld_get_image_header(i) else { continue }
            let slide = _dyld_get_image_vmaddr_slide(i)
            guard let range = virtualAddressRange(header: header, slide: slide) else { continue }
            return range
        }
        return nil
    }

    /// One frame rendered in the column layout `Thread.callStackSymbols`
    /// uses. Pure, so the format is unit-testable without a live stack.
    static func formatFrame(
        index: Int,
        image: String,
        address: UInt,
        symbol: String,
        offset: UInt
    ) -> String {
        let idxCol = String(index).padding(toLength: 3, withPad: " ", startingAt: 0)
        let imgCol = image.padding(toLength: 30, withPad: " ", startingAt: 0)
        let addrHex = "0x" + String(format: "%016lx", address)
        return "\(idxCol) \(imgCol) \(addrHex) \(symbol) + \(offset)"
    }

    // MARK: - Image range (startup, once)

    /// Union of slid `LC_SEGMENT_64` virtual ranges for `header`.
    /// Returns nil if no usable segments are found.
    private static func virtualAddressRange(
        header: UnsafePointer<mach_header>,
        slide: Int
    ) -> AppImageRange? {
        let magic = header.pointee.magic
        // Boss is arm64/x86_64 only; 32-bit Mach-O is not a supported
        // host. Bail cleanly rather than parsing LC_SEGMENT.
        guard magic == MH_MAGIC_64 || magic == MH_CIGAM_64 else { return nil }

        let headerSize = MemoryLayout<mach_header_64>.size
        let ncmds = Int(header.pointee.ncmds)
        var cmdPtr = UnsafeRawPointer(header).advanced(by: headerSize)
        var minStart: UInt = .max
        var maxEnd: UInt = 0
        let slideU = UInt(bitPattern: slide)

        for _ in 0..<ncmds {
            let cmd = cmdPtr.load(as: load_command.self)
            if cmd.cmd == LC_SEGMENT_64 {
                let seg = cmdPtr.load(as: segment_command_64.self)
                // Skip __PAGEZERO (vmaddr 0, file size 0, large vmsize).
                if seg.vmsize > 0 && (seg.filesize > 0 || seg.vmaddr != 0) {
                    let start = UInt(seg.vmaddr) &+ slideU
                    let end = start &+ UInt(seg.vmsize)
                    if start < minStart { minStart = start }
                    if end > maxEnd { maxEnd = end }
                }
            }
            cmdPtr = cmdPtr.advanced(by: Int(cmd.cmdsize))
        }

        guard minStart < maxEnd else { return nil }
        return AppImageRange(start: minStart, end: maxEnd)
    }

    // MARK: - Frame-pointer walk (suspended-thread phase, no allocation)

    private static func fillFrames(
        thread: thread_t,
        into buffer: UnsafeMutablePointer<UInt>,
        maxFrames: Int
    ) -> Int {
        var pc: UInt = 0
        var fp: UInt = 0
        guard readThreadState(thread, pc: &pc, fp: &fp) else { return 0 }

        var idx = 0
        if pc != 0 {
            buffer[idx] = pc
            idx += 1
        }

        let wordSize = UInt(MemoryLayout<UInt>.size)
        var current = fp
        while idx < maxFrames, current != 0, current % wordSize == 0 {
            // A standard frame stores {saved frame pointer, return address}
            // at [fp] and [fp + wordSize].
            var nextFp: UInt = 0
            var retAddr: UInt = 0
            guard readWord(current, &nextFp),
                  readWord(current + wordSize, &retAddr) else { break }
            if retAddr == 0 { break }
            buffer[idx] = retAddr
            idx += 1
            // The stack grows down, so each caller frame sits at a higher
            // address. Anything else means a corrupt/leaf frame — stop.
            if nextFp <= current { break }
            current = nextFp
        }
        return idx
    }

    /// Read one machine word at `addr` from our own task safely. Uses
    /// `vm_read_overwrite` (a syscall, no allocation) so a corrupt frame
    /// pointer yields a failure rather than a crash.
    private static func readWord(_ addr: UInt, _ out: inout UInt) -> Bool {
        var outSize: vm_size_t = 0
        let size = vm_size_t(MemoryLayout<UInt>.size)
        let kr = withUnsafeMutablePointer(to: &out) { dst -> kern_return_t in
            vm_read_overwrite(
                mach_task_self_,
                vm_address_t(addr),
                size,
                vm_address_t(UInt(bitPattern: UnsafeMutableRawPointer(dst))),
                &outSize
            )
        }
        return kr == KERN_SUCCESS && outSize == size
    }

    private static func readThreadState(
        _ thread: thread_t,
        pc: inout UInt,
        fp: inout UInt
    ) -> Bool {
        #if arch(arm64)
        var state = arm_thread_state64_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<arm_thread_state64_t>.stride / MemoryLayout<natural_t>.stride
        )
        let kr = withUnsafeMutablePointer(to: &state) {
            $0.withMemoryRebound(to: natural_t.self, capacity: Int(count)) {
                thread_get_state(thread, thread_state_flavor_t(ARM_THREAD_STATE64), $0, &count)
            }
        }
        guard kr == KERN_SUCCESS else { return false }
        pc = UInt(state.__pc)
        fp = UInt(state.__fp)
        return true
        #elseif arch(x86_64)
        var state = x86_thread_state64_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<x86_thread_state64_t>.stride / MemoryLayout<natural_t>.stride
        )
        let kr = withUnsafeMutablePointer(to: &state) {
            $0.withMemoryRebound(to: natural_t.self, capacity: Int(count)) {
                thread_get_state(thread, thread_state_flavor_t(x86_THREAD_STATE64), $0, &count)
            }
        }
        guard kr == KERN_SUCCESS else { return false }
        pc = UInt(state.__rip)
        fp = UInt(state.__rbp)
        return true
        #else
        return false
        #endif
    }

    private static func hex(_ v: UInt) -> String {
        "0x" + String(v, radix: 16)
    }
}

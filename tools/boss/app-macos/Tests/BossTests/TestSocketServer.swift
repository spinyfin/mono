import Darwin
import Foundation

/// Unix-domain listener whose accepted connections the test controls.
final class TestSocketServer {
    final class Peer {
        private var fd: Int32
        init(fd: Int32) { self.fd = fd }
        func readLine(timeout: TimeInterval) -> Data? {
            let deadline = Date().addingTimeInterval(timeout)
            var data = Data()
            while Date() < deadline {
                var pfd = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
                guard poll(&pfd, 1, Int32(max(1, deadline.timeIntervalSinceNow * 1000))) > 0 else { return nil }
                var byte: UInt8 = 0
                guard Darwin.read(fd, &byte, 1) == 1 else { return nil }
                if byte == 0x0A { return data }
                data.append(byte)
            }
            return nil
        }
        func sendBytes(_ data: Data) {
            data.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
                var offset = 0
                while let base = buf.baseAddress, offset < buf.count {
                    let n = Darwin.write(fd, base + offset, buf.count - offset)
                    if n <= 0 { break }
                    offset += n
                }
            }
        }
        func close() {
            guard fd >= 0 else { return }
            Darwin.close(fd)
            fd = -1
        }
    }

    private let listenFD: Int32

    init(path: String) throws {
        unlink(path)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw NSError(domain: "TestSocketServer", code: 1) }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let sunPathMax = MemoryLayout.size(ofValue: addr.sun_path)
        _ = path.withCString { cStr in
            withUnsafeMutablePointer(to: &addr.sun_path) { dst in
                memcpy(UnsafeMutableRawPointer(dst), cStr, min(strlen(cStr), sunPathMax - 1))
            }
        }
        let bound = withUnsafePointer(to: addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard bound == 0, listen(fd, 4) == 0 else {
            Darwin.close(fd)
            throw NSError(domain: "TestSocketServer", code: 2)
        }
        listenFD = fd
    }

    /// Blocks (up to `timeout`) for the next connection.
    func accept(timeout: TimeInterval) -> Peer? {
        var pfd = pollfd(fd: listenFD, events: Int16(POLLIN), revents: 0)
        guard poll(&pfd, 1, Int32(timeout * 1000)) > 0 else { return nil }
        let fd = Darwin.accept(listenFD, nil, nil)
        return fd >= 0 ? Peer(fd: fd) : nil
    }

    deinit { Darwin.close(listenFD) }
}

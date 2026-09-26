package limen.cli.os

import kotlinx.cinterop.ExperimentalForeignApi
import kotlinx.cinterop.addressOf
import kotlinx.cinterop.alloc
import kotlinx.cinterop.convert
import kotlinx.cinterop.memScoped
import kotlinx.cinterop.ptr
import kotlinx.cinterop.reinterpret
import kotlinx.cinterop.sizeOf
import kotlinx.cinterop.toKString
import kotlinx.cinterop.usePinned
import platform.linux.inet_pton
import platform.posix.AF_INET
import platform.posix.AF_INET6
import platform.posix.SOCK_STREAM
import platform.posix.SOL_SOCKET
import platform.posix.SO_RCVTIMEO
import platform.posix.SO_SNDTIMEO
import platform.posix.close
import platform.posix.connect
import platform.posix.errno
import platform.posix.recv
import platform.posix.send
import platform.posix.setsockopt
import platform.posix.sockaddr
import platform.posix.sockaddr_in
import platform.posix.sockaddr_in6
import platform.posix.socket
import platform.posix.strerror
import platform.posix.timeval

class HttpException(
    message: String,
) : Exception(message)

/**
 * Plain HTTP/1.1 to an address, one request per connection: what `limen join` needs to talk to the hub, and no
 * more. Not Ktor's client, which reaches for glibc's iconv to encode UTF-8, and a static binary has no iconv modules
 * to load (tools/ld-static). Addresses only, for the same reason: no name resolution.
 */
@OptIn(ExperimentalForeignApi::class)
object HttpLite {
    class Response(
        val status: Int,
        val body: String,
    )

    fun request(
        method: String,
        host: String,
        port: Int,
        path: String,
        body: String? = null,
        timeoutSeconds: Int = 30,
    ): Response {
        val bare = host.removePrefix("[").removeSuffix("]")
        val v6 = ':' in bare
        val fd = socket(if (v6) AF_INET6 else AF_INET, SOCK_STREAM, 0)
        if (fd < 0) throw HttpException("socket: ${error()}")
        try {
            memScoped {
                val tv = alloc<timeval>()
                tv.tv_sec = timeoutSeconds.convert()
                tv.tv_usec = 0
                setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, tv.ptr, sizeOf<timeval>().convert())
                // On Linux the send timeout also bounds connect.
                setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, tv.ptr, sizeOf<timeval>().convert())
                val connected =
                    if (v6) {
                        val addr = alloc<sockaddr_in6>()
                        addr.sin6_family = AF_INET6.convert()
                        addr.sin6_port = networkOrder(port)
                        if (inet_pton(AF_INET6, bare, addr.sin6_addr.ptr) != 1) throw HttpException("$host is not an address")
                        connect(fd, addr.ptr.reinterpret<sockaddr>(), sizeOf<sockaddr_in6>().convert())
                    } else {
                        val addr = alloc<sockaddr_in>()
                        addr.sin_family = AF_INET.convert()
                        addr.sin_port = networkOrder(port)
                        if (inet_pton(AF_INET, bare, addr.sin_addr.ptr) != 1) throw HttpException("$host is not an address")
                        connect(fd, addr.ptr.reinterpret<sockaddr>(), sizeOf<sockaddr_in>().convert())
                    }
                if (connected != 0) throw HttpException("cannot connect to $host:$port: ${error()}")
            }
            val payload = body?.encodeToByteArray() ?: ByteArray(0)
            val head =
                buildString {
                    append("$method $path HTTP/1.1\r\n")
                    append("Host: $host:$port\r\n")
                    append("Connection: close\r\n")
                    append("Accept: application/json\r\n")
                    if (body != null) append("Content-Type: application/json\r\n")
                    append("Content-Length: ${payload.size}\r\n\r\n")
                }
            sendAll(fd, head.encodeToByteArray() + payload)
            return parse(receiveAll(fd))
        } finally {
            close(fd)
        }
    }

    private fun networkOrder(port: Int): UShort = (((port and 0xff) shl 8) or ((port shr 8) and 0xff)).toUShort()

    private fun sendAll(
        fd: Int,
        bytes: ByteArray,
    ) {
        var offset = 0
        while (offset < bytes.size) {
            val n = bytes.usePinned { send(fd, it.addressOf(offset), (bytes.size - offset).convert(), 0).toInt() }
            if (n <= 0) throw HttpException("send: ${error()}")
            offset += n
        }
    }

    private fun receiveAll(fd: Int): ByteArray {
        val out = mutableListOf<ByteArray>()
        var total = 0
        val buffer = ByteArray(16 * 1024)
        while (true) {
            val n = buffer.usePinned { recv(fd, it.addressOf(0), buffer.size.convert(), 0).toInt() }
            if (n < 0) throw HttpException("no answer: ${error()}")
            if (n == 0) break
            out += buffer.copyOf(n)
            total += n
            if (total > 1024 * 1024) throw HttpException("the answer is too large")
        }
        val all = ByteArray(total)
        var at = 0
        for (p in out) {
            p.copyInto(all, at)
            at += p.size
        }
        return all
    }

    /** Status line, headers, and a body that may come chunked; decoded as UTF-8 only at the end. */
    fun parse(raw: ByteArray): Response {
        val split = indexOf(raw, "\r\n\r\n".encodeToByteArray(), 0)
        if (split < 0) throw HttpException("not an HTTP answer")
        val head = raw.copyOfRange(0, split).decodeToString().split("\r\n")
        val status =
            head
                .first()
                .split(' ')
                .getOrNull(1)
                ?.toIntOrNull() ?: throw HttpException("not an HTTP answer")
        val chunked = head.drop(1).any { it.lowercase().replace(" ", "") == "transfer-encoding:chunked" }
        val body = raw.copyOfRange(split + 4, raw.size)
        return Response(status, (if (chunked) dechunk(body) else body).decodeToString())
    }

    private fun dechunk(body: ByteArray): ByteArray {
        val out = mutableListOf<Byte>()
        var at = 0
        val crlf = "\r\n".encodeToByteArray()
        while (at < body.size) {
            val end = indexOf(body, crlf, at)
            if (end < 0) break
            val size =
                body
                    .copyOfRange(at, end)
                    .decodeToString()
                    .substringBefore(';')
                    .trim()
                    .toIntOrNull(16) ?: break
            if (size == 0) break
            val start = end + 2
            for (i in start until minOf(start + size, body.size)) out += body[i]
            at = start + size + 2
        }
        return out.toByteArray()
    }

    private fun indexOf(
        haystack: ByteArray,
        needle: ByteArray,
        from: Int,
    ): Int {
        outer@ for (i in from..haystack.size - needle.size) {
            for (j in needle.indices) if (haystack[i + j] != needle[j]) continue@outer
            return i
        }
        return -1
    }

    private fun error(): String = strerror(errno)?.toKString() ?: "errno $errno"
}

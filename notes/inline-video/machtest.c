// Pre-spike: can a plain process register a named Mach service with
// bootstrap_check_in, and can another process send it an IOSurface?
//   ./machtest serve            -> prints the service name, waits for one surface
//   ./machtest send <name>      -> makes a 64x64 BGRA surface, sends it
#include <CoreFoundation/CoreFoundation.h>
#include <IOSurface/IOSurface.h>
#include <mach/mach.h>
#include <servers/bootstrap.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

typedef struct {
    mach_msg_header_t header;
    mach_msg_body_t body;
    mach_msg_port_descriptor_t surface;
    uint32_t slot;
} surface_msg_t;

typedef struct {
    surface_msg_t msg;
    mach_msg_trailer_t trailer;
} surface_rcv_t;

static int serve(void) {
    char name[128];
    snprintf(name, sizeof name, "com.pepper.test.%d", getpid());
    mach_port_t port = MACH_PORT_NULL;
    kern_return_t kr = bootstrap_check_in(bootstrap_port, name, &port);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "bootstrap_check_in(%s) failed: %d %s\n", name, kr,
                bootstrap_strerror(kr));
        return 1;
    }
    printf("%s\n", name);
    fflush(stdout);

    surface_rcv_t rcv = {0};
    kr = mach_msg(&rcv.msg.header, MACH_RCV_MSG | MACH_RCV_TIMEOUT, 0,
                  sizeof rcv, port, 30000, MACH_PORT_NULL);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "mach_msg receive failed: %d %s\n", kr, mach_error_string(kr));
        return 1;
    }
    IOSurfaceRef s = IOSurfaceLookupFromMachPort(rcv.msg.surface.name);
    mach_port_deallocate(mach_task_self(), rcv.msg.surface.name);
    if (!s) {
        fprintf(stderr, "IOSurfaceLookupFromMachPort returned NULL\n");
        return 1;
    }
    IOSurfaceLock(s, kIOSurfaceLockReadOnly, NULL);
    uint32_t first = *(uint32_t *)IOSurfaceGetBaseAddress(s);
    IOSurfaceUnlock(s, kIOSurfaceLockReadOnly, NULL);
    printf("OK: got surface %zux%zu, slot %u, first pixel 0x%08x\n",
           IOSurfaceGetWidth(s), IOSurfaceGetHeight(s), rcv.msg.slot, first);
    CFRelease(s);
    return 0;
}

static int send_surface(const char *name) {
    mach_port_t port = MACH_PORT_NULL;
    kern_return_t kr = bootstrap_look_up(bootstrap_port, name, &port);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "bootstrap_look_up(%s) failed: %d %s\n", name, kr,
                bootstrap_strerror(kr));
        return 1;
    }
    int w = 64, h = 64, bpe = 4;
    CFMutableDictionaryRef props = CFDictionaryCreateMutable(
        NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    uint32_t fmt = 'BGRA';
    CFNumberRef nw = CFNumberCreate(NULL, kCFNumberIntType, &w);
    CFNumberRef nh = CFNumberCreate(NULL, kCFNumberIntType, &h);
    CFNumberRef nb = CFNumberCreate(NULL, kCFNumberIntType, &bpe);
    CFNumberRef nf = CFNumberCreate(NULL, kCFNumberSInt32Type, &fmt);
    CFDictionarySetValue(props, kIOSurfaceWidth, nw);
    CFDictionarySetValue(props, kIOSurfaceHeight, nh);
    CFDictionarySetValue(props, kIOSurfaceBytesPerElement, nb);
    CFDictionarySetValue(props, kIOSurfacePixelFormat, nf);
    IOSurfaceRef s = IOSurfaceCreate(props);
    if (!s) {
        fprintf(stderr, "IOSurfaceCreate failed\n");
        return 1;
    }
    IOSurfaceLock(s, 0, NULL);
    *(uint32_t *)IOSurfaceGetBaseAddress(s) = 0xdeadbeef;
    IOSurfaceUnlock(s, 0, NULL);

    surface_msg_t msg = {0};
    msg.header.msgh_bits =
        MACH_MSGH_BITS_SET(MACH_MSG_TYPE_COPY_SEND, 0, 0, MACH_MSGH_BITS_COMPLEX);
    msg.header.msgh_size = sizeof msg;
    msg.header.msgh_remote_port = port;
    msg.body.msgh_descriptor_count = 1;
    msg.surface.name = IOSurfaceCreateMachPort(s);
    msg.surface.disposition = MACH_MSG_TYPE_MOVE_SEND;
    msg.surface.type = MACH_MSG_PORT_DESCRIPTOR;
    msg.slot = 2;
    kr = mach_msg(&msg.header, MACH_SEND_MSG | MACH_SEND_TIMEOUT, sizeof msg, 0,
                  MACH_PORT_NULL, 5000, MACH_PORT_NULL);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "mach_msg send failed: %d %s\n", kr, mach_error_string(kr));
        return 1;
    }
    printf("sent\n");
    CFRelease(s);
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 2 && strcmp(argv[1], "serve") == 0) return serve();
    if (argc >= 3 && strcmp(argv[1], "send") == 0) return send_surface(argv[2]);
    fprintf(stderr, "usage: %s serve | send <name>\n", argv[0]);
    return 2;
}

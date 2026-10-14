// Test client for Pepper: places a box with OSC 777;pepper, hands Pepper 3
// IOSurfaces over its Mach port with the same token, then draws a moving bar
// into each in turn and presents it.
//
//   clang -O2 -o pepper_play pepper_play.c -framework IOSurface -framework CoreFoundation
//   ./pepper_play [-k] [seconds=10] [fps=30]
//
// Run it in a Pepper tab (it needs PEPPER_PORT, and the escape sequence must
// reach Pepper). -k exits without detaching or removing the box, as a
// crashed helper would: Pepper should clear the frame by itself.
// Messages match macos/Sources/Features/Pepper/PepperServer.swift.
#include <CoreFoundation/CoreFoundation.h>
#include <IOSurface/IOSurface.h>
#include <mach/mach.h>
#include <servers/bootstrap.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

enum { MSG_ATTACH = 0x70700001, MSG_PRESENT = 0x70700002, MSG_DETACH = 0x70700003 };
enum { SLOTS = 3, WIDTH = 640, HEIGHT = 360, TOKEN_SIZE = 64 };

typedef struct {
    mach_msg_header_t header;
    mach_msg_body_t body;
    mach_msg_port_descriptor_t surfaces[SLOTS];
    char token[TOKEN_SIZE];
} attach_msg_t;

typedef struct {
    mach_msg_header_t header;
    char token[TOKEN_SIZE];
    uint32_t slot;
} present_msg_t;

typedef struct {
    mach_msg_header_t header;
    char token[TOKEN_SIZE];
} detach_msg_t;

static mach_port_t server;
static char token[TOKEN_SIZE];
static volatile sig_atomic_t resized = 1;

static int send_msg(mach_msg_header_t *h, mach_msg_size_t size) {
    h->msgh_remote_port = server;
    h->msgh_size = size;
    kern_return_t kr = mach_msg(h, MACH_SEND_MSG | MACH_SEND_TIMEOUT, size, 0,
                                MACH_PORT_NULL, 1000, MACH_PORT_NULL);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "mach_msg send failed: %s\n", mach_error_string(kr));
        return -1;
    }
    return 0;
}

// A 16:9 box of cells, 2 cells in from the top left, half the window wide.
static void place(void) {
    struct winsize ws = {0};
    ioctl(STDOUT_FILENO, TIOCGWINSZ, &ws);
    int cols = ws.ws_col > 8 ? ws.ws_col / 2 : 40;
    int rows = 0;
    if (ws.ws_xpixel && ws.ws_ypixel) {
        double cw = (double)ws.ws_xpixel / ws.ws_col, ch = (double)ws.ws_ypixel / ws.ws_row;
        rows = (int)(cols * cw * 9 / 16 / ch + 0.5);
    }
    if (rows < 1) rows = cols / 4;
    printf("\033]777;pepper;place;%s;2;2;%d;%d\033\\", token, cols, rows);
    fflush(stdout);
}

static void on_winch(int sig) {
    (void)sig;
    resized = 1;
}

static IOSurfaceRef make_surface(void) {
    int w = WIDTH, h = HEIGHT, bpe = 4;
    uint32_t fmt = 'BGRA';
    CFMutableDictionaryRef props = CFDictionaryCreateMutable(
        NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFNumberRef nw = CFNumberCreate(NULL, kCFNumberIntType, &w);
    CFNumberRef nh = CFNumberCreate(NULL, kCFNumberIntType, &h);
    CFNumberRef nb = CFNumberCreate(NULL, kCFNumberIntType, &bpe);
    CFNumberRef nf = CFNumberCreate(NULL, kCFNumberSInt32Type, &fmt);
    CFDictionarySetValue(props, kIOSurfaceWidth, nw);
    CFDictionarySetValue(props, kIOSurfaceHeight, nh);
    CFDictionarySetValue(props, kIOSurfaceBytesPerElement, nb);
    CFDictionarySetValue(props, kIOSurfacePixelFormat, nf);
    IOSurfaceRef s = IOSurfaceCreate(props);
    CFRelease(nw), CFRelease(nh), CFRelease(nb), CFRelease(nf), CFRelease(props);
    return s;
}

// A dark background tinted per slot, and a white bar that moves with time.
static void draw(IOSurfaceRef s, int slot, long frame) {
    static const uint32_t tint[SLOTS] = {0xff401010, 0xff104010, 0xff101040};
    IOSurfaceLock(s, 0, NULL);
    uint8_t *base = IOSurfaceGetBaseAddress(s);
    size_t stride = IOSurfaceGetBytesPerRow(s);
    int bar = (int)(frame * 8 % WIDTH);
    for (int y = 0; y < HEIGHT; y++) {
        uint32_t *row = (uint32_t *)(base + y * stride);
        for (int x = 0; x < WIDTH; x++)
            row[x] = (x >= bar && x < bar + 24) ? 0xffffffff : tint[slot];
    }
    IOSurfaceUnlock(s, 0, NULL);
}

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
    int crash = argc > 1 && strcmp(argv[1], "-k") == 0;
    if (crash) argc--, argv++;
    double seconds = argc > 1 ? atof(argv[1]) : 10;
    double fps = argc > 2 ? atof(argv[2]) : 30;
    const char *name = getenv("PEPPER_PORT");
    if (!name) {
        fprintf(stderr, "PEPPER_PORT is not set: run this in a Pepper tab\n");
        return 1;
    }
    kern_return_t kr = bootstrap_look_up(bootstrap_port, name, &server);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "bootstrap_look_up(%s): %s\n", name, bootstrap_strerror(kr));
        return 1;
    }

    // 32 random hex digits.
    uint8_t random[16];
    arc4random_buf(random, sizeof random);
    for (int i = 0; i < 16; i++) snprintf(token + 2 * i, 3, "%02x", random[i]);

    // Our port: Pepper watches it to learn when we die.
    mach_port_t client;
    mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &client);

    IOSurfaceRef surfaces[SLOTS];
    attach_msg_t attach = {0};
    attach.header.msgh_bits = MACH_MSGH_BITS_SET(
        MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND, 0, MACH_MSGH_BITS_COMPLEX);
    attach.header.msgh_local_port = client;
    attach.header.msgh_id = MSG_ATTACH;
    attach.body.msgh_descriptor_count = SLOTS;
    for (int i = 0; i < SLOTS; i++) {
        surfaces[i] = make_surface();
        if (!surfaces[i]) {
            fprintf(stderr, "IOSurfaceCreate failed\n");
            return 1;
        }
        attach.surfaces[i].name = IOSurfaceCreateMachPort(surfaces[i]);
        attach.surfaces[i].disposition = MACH_MSG_TYPE_MOVE_SEND;
        attach.surfaces[i].type = MACH_MSG_PORT_DESCRIPTOR;
    }
    memcpy(attach.token, token, sizeof token);
    if (send_msg(&attach.header, sizeof attach)) return 1;

    signal(SIGWINCH, on_winch);
    long frames = (long)(seconds * fps), in_use = 0;
    double start = now(), late = 0;
    for (long f = 1; f <= frames; f++) {
        if (resized) {
            resized = 0;
            place();
        }
        int slot = f % SLOTS;
        if (IOSurfaceIsInUse(surfaces[slot])) in_use++;
        draw(surfaces[slot], slot, f);
        present_msg_t present = {0};
        present.header.msgh_bits = MACH_MSGH_BITS_SET(MACH_MSG_TYPE_COPY_SEND, 0, 0, 0);
        present.header.msgh_id = MSG_PRESENT;
        memcpy(present.token, token, sizeof token);
        present.slot = slot;
        if (send_msg(&present.header, sizeof present)) return 1;

        double wait = start + f / fps - now();
        if (wait > 0) usleep((useconds_t)(wait * 1e6));
        else late -= wait;
    }

    if (crash) {
        fprintf(stderr, "exiting without detach: the frame should clear\n");
        _exit(0);
    }

    detach_msg_t detach = {0};
    detach.header.msgh_bits = MACH_MSGH_BITS_SET(MACH_MSG_TYPE_COPY_SEND, 0, 0, 0);
    detach.header.msgh_id = MSG_DETACH;
    memcpy(detach.token, token, sizeof token);
    send_msg(&detach.header, sizeof detach);
    printf("\033]777;pepper;remove;%s\033\\", token);
    fflush(stdout);

    printf("%ld frames in %.1f s, %ld written while still in use, %.0f ms late in all\n",
           frames, now() - start, in_use, late * 1000);
    return 0;
}

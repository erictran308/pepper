// Prints the system's Now Playing app (pid) and its info. Loaded into Apple's
// /usr/bin/perl, which may read MediaRemote (a plain program may not).
#import <Foundation/Foundation.h>
#include <dlfcn.h>
#include <stdio.h>

typedef void (*GetInfo)(dispatch_queue_t, void (^)(NSDictionary *));
typedef void (*GetPID)(dispatch_queue_t, void (^)(int));

void np_dump(void *a, void *b) {
    (void)a; (void)b;
    void *mr = dlopen("/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote", RTLD_NOW);
    GetInfo getInfo = (GetInfo)dlsym(mr, "MRMediaRemoteGetNowPlayingInfo");
    GetPID getPID = (GetPID)dlsym(mr, "MRMediaRemoteGetNowPlayingApplicationPID");
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    getPID(dispatch_get_global_queue(0, 0), ^(int pid) {
        printf("now playing pid: %d\n", pid);
        dispatch_semaphore_signal(done);
    });
    dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC));
    getInfo(dispatch_get_global_queue(0, 0), ^(NSDictionary *info) {
        printf("info: %lu keys\n", (unsigned long)info.count);
        for (NSString *key in [info.allKeys sortedArrayUsingSelector:@selector(compare:)]) {
            id value = info[key];
            if ([value isKindOfClass:NSData.class])
                printf("  %s: <%lu bytes>\n", key.UTF8String, (unsigned long)[value length]);
            else
                printf("  %s: %.80s\n", key.UTF8String, [[value description] UTF8String]);
        }
        dispatch_semaphore_signal(done);
    });
    dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC));
    fflush(stdout);
}

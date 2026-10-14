// Prints the name Now Playing (MediaRemote) gives the app playing now, and
// what AppKit and the file system call it. Build and run as nowplaying_dump.m:
// clang -dynamiclib -fobjc-arc -framework Foundation -framework AppKit -o name.dylib nowplaying_name.m
// /usr/bin/perl nowplaying_dump.pl "$PWD/name.dylib"
#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>
#include <dlfcn.h>
#include <stdio.h>
typedef void (*GetPID)(dispatch_queue_t, void (^)(int));
typedef void (*GetClient)(dispatch_queue_t, void (^)(id));
typedef NSString *(*ClientStr)(id);
void np_dump(void *a, void *b) {
    void *mr = dlopen("/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote", RTLD_NOW);
    GetPID getPID = (GetPID)dlsym(mr, "MRMediaRemoteGetNowPlayingApplicationPID");
    GetClient getClient = (GetClient)dlsym(mr, "MRMediaRemoteGetNowPlayingClient");
    ClientStr displayName = (ClientStr)dlsym(mr, "MRNowPlayingClientGetDisplayName");
    ClientStr bundleID = (ClientStr)dlsym(mr, "MRNowPlayingClientGetBundleIdentifier");
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    __block int thepid = 0;
    getPID(dispatch_get_global_queue(0, 0), ^(int pid) { thepid = pid; dispatch_semaphore_signal(done); });
    dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC));
    printf("pid %d\n", thepid);
    if (getClient) {
        getClient(dispatch_get_global_queue(0, 0), ^(id client) {
            printf("client: %s\n", [[client description] UTF8String]);
            if (displayName) printf("MR displayName: %s\n", [displayName(client) UTF8String]);
            if (bundleID) printf("MR bundleID: %s\n", [bundleID(client) UTF8String]);
            dispatch_semaphore_signal(done);
        });
        dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC));
    }
    NSRunningApplication *app = [NSRunningApplication runningApplicationWithProcessIdentifier:thepid];
    printf("NSRunningApplication localizedName: %s\n", app.localizedName.UTF8String);
    printf("bundleURL: %s\n", app.bundleURL.path.UTF8String);
    if (app.bundleURL) printf("FileManager displayName: %s\n", [[NSFileManager.defaultManager displayNameAtPath:app.bundleURL.path] UTF8String]);
    fflush(stdout);
}

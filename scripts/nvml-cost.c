/* SPDX-License-Identifier: GPL-3.0-or-later */
/* How much memory nvmlInit() costs, measured in a program that does nothing
 * else. Built to answer one question about fanctl's resident set: the extra
 * ~25 MB that appears when a GPU source is configured - does it belong to the
 * program or to the NVIDIA driver?
 *
 *   cc -O2 -o nvml-cost nvml-cost.c -ldl && ./nvml-cost
 */
#include <dlfcn.h>
#include <stdio.h>
#include <string.h>

static long rss_kb(void)
{
    FILE *statm = fopen("/proc/self/statm", "r");
    long size = 0, resident = 0;
    if (statm) {
        if (fscanf(statm, "%ld %ld", &size, &resident) != 2)
            resident = 0;
        fclose(statm);
    }
    return resident * 4;
}

static void report(const char *stage)
{
    printf("%-34s %6ld kB\n", stage, rss_kb());
}

int main(void)
{
    typedef int (*init_fn)(void);
    typedef int (*shutdown_fn)(void);
    typedef int (*count_fn)(unsigned *);
    typedef int (*handle_fn)(unsigned, void **);
    typedef int (*temp_fn)(void *, int, unsigned *);

    report("start");

    void *lib = dlopen("libnvidia-ml.so.1", RTLD_NOW | RTLD_GLOBAL);
    if (!lib) {
        fprintf(stderr, "dlopen: %s\n", dlerror());
        return 1;
    }
    report("after dlopen");

    init_fn init = (init_fn)dlsym(lib, "nvmlInit_v2");
    shutdown_fn shutdown = (shutdown_fn)dlsym(lib, "nvmlShutdown");
    count_fn count = (count_fn)dlsym(lib, "nvmlDeviceGetCount_v2");
    handle_fn handle = (handle_fn)dlsym(lib, "nvmlDeviceGetHandleByIndex_v2");
    temp_fn temp = (temp_fn)dlsym(lib, "nvmlDeviceGetTemperature");
    if (!init || !shutdown || !count || !handle || !temp) {
        fprintf(stderr, "missing symbols\n");
        return 1;
    }

    if (init() != 0) {
        fprintf(stderr, "nvmlInit failed\n");
        return 1;
    }
    report("after nvmlInit");

    unsigned devices = 0;
    if (count(&devices) != 0) {
        fprintf(stderr, "nvmlDeviceGetCount failed\n");
        return 1;
    }
    for (unsigned i = 0; i < devices; i++) {
        void *dev = NULL;
        unsigned celsius = 0;
        if (handle(i, &dev) == 0 && temp(dev, 0, &celsius) == 0)
            printf("  gpu %u: %u C\n", i, celsius);
    }
    report("after reading every GPU");

    shutdown();
    report("after nvmlShutdown");
    return 0;
}

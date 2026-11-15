/* SPDX-License-Identifier: GPL-3.0-or-later */
/* What a GPU sample costs, step by step, resident versus reopened.
 *
 * fanctl keeps the NVML handle for the life of the process. The alternative is
 * to open it, read, and close it every cycle, which returns the driver's ~25 MB
 * (dlclose unmaps it) at the price of repeating the setup. This measures both
 * sides: the individual calls that a resident reader makes per cycle, and the
 * setup a per-cycle reader would pay.
 *
 *   cc -O2 -o nvml-steps nvml-steps.c -ldl && ./nvml-steps
 */
#define _POSIX_C_SOURCE 200809L
#include <dlfcn.h>
#include <stdio.h>
#include <time.h>

static double now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

static long rss_kb(void)
{
    FILE *f = fopen("/proc/self/statm", "r");
    long size = 0, resident = 0;
    if (f) {
        if (fscanf(f, "%ld %ld", &size, &resident) != 2)
            resident = 0;
        fclose(f);
    }
    return resident * 4;
}

int main(void)
{
    typedef int (*init_fn)(void);
    typedef int (*shutdown_fn)(void);
    typedef int (*count_fn)(unsigned *);
    typedef int (*handle_fn)(unsigned, void **);
    typedef int (*temp_fn)(void *, int, unsigned *);
    typedef int (*name_fn)(void *, char *, unsigned);

    const int N = 200;

    void *lib = dlopen("libnvidia-ml.so.1", RTLD_NOW | RTLD_GLOBAL);
    if (!lib) {
        fprintf(stderr, "dlopen: %s\n", dlerror());
        return 1;
    }
    init_fn init = (init_fn)dlsym(lib, "nvmlInit_v2");
    shutdown_fn shutdown = (shutdown_fn)dlsym(lib, "nvmlShutdown");
    count_fn count = (count_fn)dlsym(lib, "nvmlDeviceGetCount_v2");
    handle_fn handle = (handle_fn)dlsym(lib, "nvmlDeviceGetHandleByIndex_v2");
    temp_fn temp = (temp_fn)dlsym(lib, "nvmlDeviceGetTemperature");
    name_fn name = (name_fn)dlsym(lib, "nvmlDeviceGetName");
    if (!init || !shutdown || !count || !handle || !temp || !name) {
        fprintf(stderr, "missing symbols\n");
        return 1;
    }

    /* --- resident: what fanctl does now ------------------------------- */
    double t0 = now_ms();
    init();
    double t_init = now_ms() - t0;

    unsigned devices = 0;
    count(&devices);

    void *devs[8];
    char buf[96];
    for (unsigned i = 0; i < devices && i < 8; i++)
        handle(i, &devs[i]);

    t0 = now_ms();
    for (int i = 0; i < N; i++)
        count(&devices);
    double per_count = (now_ms() - t0) / N;

    t0 = now_ms();
    for (int i = 0; i < N; i++)
        for (unsigned d = 0; d < devices; d++)
            handle(d, &devs[d]);
    double per_handle = (now_ms() - t0) / N / devices;

    t0 = now_ms();
    for (int i = 0; i < N; i++)
        for (unsigned d = 0; d < devices; d++)
            temp(devs[d], 0, NULL);
    double per_temp = (now_ms() - t0) / N / devices;

    t0 = now_ms();
    for (int i = 0; i < N; i++)
        for (unsigned d = 0; d < devices; d++)
            name(devs[d], buf, sizeof buf);
    double per_name = (now_ms() - t0) / N / devices;

    printf("resident, one init, %u devices\n", devices);
    printf("  nvmlInit                    %8.3f ms  (once)\n", t_init);
    printf("  device_count                %8.3f ms\n", per_count);
    printf("  device_by_index             %8.3f ms per device\n", per_handle);
    printf("  temperature                 %8.3f ms per device\n", per_temp);
    printf("  device name                 %8.3f ms per device\n", per_name);
    printf("  --- per cycle, as written:  %8.3f ms\n",
           per_count + (per_handle + per_temp + per_name) * devices);
    printf("  --- per cycle, handles and names cached: %8.3f ms\n",
           per_count + per_temp * devices);
    printf("  RSS                         %8ld kB\n", rss_kb());

    shutdown();
    dlclose(lib);
    printf("after shutdown + dlclose, RSS %8ld kB\n\n", rss_kb());

    /* --- reopened: what a per-cycle reader would pay ------------------ */
    const int R = 20;
    double total = 0;
    t0 = now_ms();
    for (int r = 0; r < R; r++) {
        double a = now_ms();
        void *l = dlopen("libnvidia-ml.so.1", RTLD_NOW | RTLD_GLOBAL);
        init_fn i2 = (init_fn)dlsym(l, "nvmlInit_v2");
        shutdown_fn s2 = (shutdown_fn)dlsym(l, "nvmlShutdown");
        count_fn c2 = (count_fn)dlsym(l, "nvmlDeviceGetCount_v2");
        handle_fn h2 = (handle_fn)dlsym(l, "nvmlDeviceGetHandleByIndex_v2");
        temp_fn t2 = (temp_fn)dlsym(l, "nvmlDeviceGetTemperature");
        name_fn n2 = (name_fn)dlsym(l, "nvmlDeviceGetName");
        i2();
        unsigned n = 0;
        c2(&n);
        for (unsigned d = 0; d < n; d++) {
            void *dev = NULL;
            h2(d, &dev);
            t2(dev, 0, NULL);
            n2(dev, buf, sizeof buf);
        }
        s2();
        dlclose(l);
        total += now_ms() - a;
    }
    printf("reopened every cycle, %d rounds\n", R);
    printf("  open + init + read + shutdown + close   %8.3f ms\n", total / R);
    printf("  RSS after the last close                %8ld kB\n", rss_kb());
    return 0;
}

/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Should fanctl keep NVML loaded, or open it for each sample?
 *
 * Opens and closes the driver library in a loop and reports, per iteration,
 * the resident set and how long each step took. If the memory comes back after
 * nvmlShutdown + dlclose, sampling per cycle would cost nothing but time; if it
 * does not, a resident handle is cheaper than reloading.
 *
 *   cc -O2 -o nvml-residency nvml-residency.c -ldl && ./nvml-residency
 */
#define _POSIX_C_SOURCE 200809L
#include <dlfcn.h>
#include <stdio.h>
#include <time.h>

struct mem {
    long rss, pss, anon;
};

static struct mem sample(void)
{
    struct mem m = {0, 0, 0};
    FILE *f = fopen("/proc/self/smaps_rollup", "r");
    char line[128];
    if (!f)
        return m;
    while (fgets(line, sizeof line, f)) {
        long v;
        if (sscanf(line, "Rss: %ld", &v) == 1)
            m.rss = v;
        else if (sscanf(line, "Pss: %ld", &v) == 1)
            m.pss = v;
        else if (sscanf(line, "Pss_Anon: %ld", &v) == 1)
            m.anon = v;
    }
    fclose(f);
    return m;
}

static double now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

int main(void)
{
    typedef int (*init_fn)(void);
    typedef int (*shutdown_fn)(void);
    typedef int (*count_fn)(unsigned *);
    typedef int (*handle_fn)(unsigned, void **);
    typedef int (*temp_fn)(void *, int, unsigned *);

    printf("%-6s %8s %8s %8s %9s %9s %9s\n",
           "round", "RSS kB", "PSS kB", "anon kB", "dlopen ms", "init ms", "read ms");
    printf("%-6s %8s %8s %8s %9s %9s %9s\n",
           "------", "------", "------", "------", "---------", "-------", "-------");

    struct mem m = sample();
    printf("%-6s %8ld %8ld %8ld\n", "start", m.rss, m.pss, m.anon);

    for (int round = 1; round <= 8; round++) {
        double t0 = now_ms();
        void *lib = dlopen("libnvidia-ml.so.1", RTLD_NOW | RTLD_GLOBAL);
        double t1 = now_ms();
        if (!lib) {
            fprintf(stderr, "dlopen: %s\n", dlerror());
            return 1;
        }

        init_fn init = (init_fn)dlsym(lib, "nvmlInit_v2");
        shutdown_fn shutdown = (shutdown_fn)dlsym(lib, "nvmlShutdown");
        count_fn count = (count_fn)dlsym(lib, "nvmlDeviceGetCount_v2");
        handle_fn handle = (handle_fn)dlsym(lib, "nvmlDeviceGetHandleByIndex_v2");
        temp_fn temp = (temp_fn)dlsym(lib, "nvmlDeviceGetTemperature");
        if (!init || !shutdown || !count || !handle || !temp) {
            fprintf(stderr, "missing symbols\n");
            return 1;
        }

        double t2 = now_ms();
        if (init() != 0) {
            fprintf(stderr, "nvmlInit failed\n");
            return 1;
        }
        double t3 = now_ms();

        unsigned devices = 0;
        count(&devices);
        for (unsigned i = 0; i < devices; i++) {
            void *dev = NULL;
            unsigned celsius = 0;
            if (handle(i, &dev) == 0)
                temp(dev, 0, &celsius);
        }
        double t4 = now_ms();

        shutdown();
        dlclose(lib);

        m = sample();
        printf("%-6d %8ld %8ld %8ld %9.2f %9.2f %9.2f\n", round, m.rss, m.pss,
               m.anon, t1 - t0, t3 - t2, t4 - t3);
    }

    printf("\n%s\n", "If RSS does not fall back to a few megabytes after the close, the");
    printf("%s\n", "driver keeps its heap and reloading each cycle saves nothing.");
    return 0;
}

/* SPDX-License-Identifier: MIT */
/* Non-UI unit test for the Mesa queue worker's platform placement hook. */
#define _GNU_SOURCE
#include <assert.h>
#include <pthread.h>
#include <stdbool.h>
#include "u_queue_cpu.h"

static cpu_set_t graphics, background;

static void *worker(void *unused)
{
   (void)unused;
   cpu_set_t actual;
   assert(!sched_getaffinity(0, sizeof(actual), &actual));
   assert(CPU_EQUAL(&actual, &graphics));
   util_queue_apply_background_policy();
   assert(!sched_getaffinity(0, sizeof(actual), &actual));
   assert(CPU_EQUAL(&actual, &background));
   assert(sched_getscheduler(0) == SCHED_BATCH);
   return NULL;
}

int main(void)
{
   cpu_set_t mask;
   const char *invalid[] = { "", "-1", "1,", "1--2", "2-1", "1-2x", "1024", "9999999999999999999999999" };
   for (unsigned i = 0; i < sizeof(invalid) / sizeof(invalid[0]); ++i)
      assert(!util_queue_parse_cpus(invalid[i], &mask));
   assert(util_queue_parse_cpus("0-2,5,7-8", &mask));
   assert(CPU_COUNT(&mask) == 6 && CPU_ISSET(8, &mask) && !CPU_ISSET(6, &mask));
   cpu_set_t original;
   assert(!sched_getaffinity(0, sizeof(original), &original));
   int first = -1, last = -1;
   for (int cpu = 0; cpu < CPU_SETSIZE; cpu++) {
      if (!CPU_ISSET(cpu, &original)) continue;
      if (first == -1) first = cpu;
      last = cpu;
   }
   if (first == last) { puts("SKIP: need two allowed CPUs for worker isolation"); return 77; }
   CPU_ZERO(&graphics); CPU_SET(first, &graphics);
   CPU_ZERO(&background); CPU_SET(last, &background);
   char list[32]; snprintf(list, sizeof(list), "%d", last);
   assert(!setenv("MESA_BACKGROUND_CPUS", list, 1));
   assert(!unsetenv("MESA_BACKGROUND_CPUSET"));
   assert(!sched_setaffinity(0, sizeof(graphics), &graphics));
   pthread_t thread;
   assert(!pthread_create(&thread, NULL, worker, NULL));
   assert(!pthread_join(thread, NULL));
   assert(!sched_getaffinity(0, sizeof(mask), &mask));
   assert(CPU_EQUAL(&mask, &graphics));
   assert(!sched_setaffinity(0, sizeof(original), &original));
   puts("PASS: CPU-list validation, inherited graphics mask, worker background override, creator unchanged");
   return 0;
}

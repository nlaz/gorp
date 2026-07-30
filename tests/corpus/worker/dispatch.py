"""Worker dispatch loop: claim jobs, run handlers, report outcomes."""

import signal
import time

SHUTDOWN_GRACE_SECONDS = 30


class Dispatcher:
    """Pulls jobs off the queue and routes them to registered handlers."""

    def __init__(self, queue, registry, max_in_flight=8):
        self.queue = queue
        self.registry = registry
        self.max_in_flight = max_in_flight
        self.handlers = {}
        self.draining = False
        self.in_flight = []

    def register_handler(self, job_kind, fn):
        if job_kind in self.handlers:
            raise ValueError(f"handler already registered for {job_kind}")
        self.handlers[job_kind] = fn

    def run_forever(self, poll_interval=0.25):
        self._install_signal_handlers()
        while not self.draining or self.in_flight:
            if self.draining or len(self.in_flight) >= self.max_in_flight:
                time.sleep(poll_interval)
                self._reap()
                continue
            job = self.queue.dequeue()
            if job is None:
                time.sleep(poll_interval)
                continue
            self._start(job)

    def _start(self, job):
        handler = self.handlers.get(job.kind)
        if handler is None:
            self.registry.increment("jobs_unroutable")
            self.queue.ack(job.id)
            return
        started = time.monotonic()
        self.in_flight.append((job, started, handler))

    def _reap(self):
        """Ack finished work and hand failures back for retry."""
        still_running = []
        for job, started, handler in self.in_flight:
            try:
                handler(job)
            except Exception:
                self.registry.increment("jobs_failed")
                self.queue.requeue_expired(job.id)
                continue
            self.registry.observe_latency(
                "job_duration", int((time.monotonic() - started) * 1000)
            )
            self.queue.ack(job.id)
        self.in_flight = still_running

    def _install_signal_handlers(self):
        """SIGTERM starts a drain instead of killing in-flight work, so a
        rolling deploy does not lose jobs that were mid-handler."""

        def on_term(_signum, _frame):
            self.draining = True

        signal.signal(signal.SIGTERM, on_term)
        signal.signal(signal.SIGINT, on_term)

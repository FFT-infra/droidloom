package com.android.droidloom.sensorprobe;

import android.app.Activity;
import android.app.Instrumentation;
import android.hardware.Sensor;
import android.hardware.SensorEvent;
import android.hardware.SensorEventListener2;
import android.hardware.SensorManager;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/** Public API regression for an ordinary app's default accelerometer and event stream. */
public final class Probe extends Instrumentation {
    @Override public void onCreate(Bundle arguments) {
        super.onCreate(arguments);
        start();
    }

    @Override public void onStart() {
        Bundle result = new Bundle();
        SensorManager manager = getTargetContext().getSystemService(SensorManager.class);
        HandlerThread callbacks = new HandlerThread("sensor-probe");
        Listener listener = new Listener();
        int resultCode = Activity.RESULT_CANCELED;
        try {
            if (manager == null) throw new AssertionError("sensorManagerNull");
            Sensor sensor = manager.getDefaultSensor(Sensor.TYPE_ACCELEROMETER);
            if (sensor == null) throw new AssertionError("defaultSensorNull");
            if (sensor.isDynamicSensor()) throw new AssertionError("defaultSensorDynamic");
            if (sensor.isWakeUpSensor()) throw new AssertionError("defaultSensorWakeup");
            result.putString("sensor_name", sensor.getName());
            result.putInt("sensor_count", manager.getSensorList(Sensor.TYPE_ALL).size());
            callbacks.start();
            if (!manager.registerListener(listener, sensor, SensorManager.SENSOR_DELAY_GAME,
                    new Handler(callbacks.getLooper()))) {
                throw new AssertionError("registerListenerFailed");
            }
            if (!listener.samples.await(5, TimeUnit.SECONDS)) {
                throw new AssertionError("sensorEventsTimedOut");
            }
            if (listener.error != null) throw new AssertionError(listener.error);
            if (!manager.flush(listener) || !listener.flushed.await(2, TimeUnit.SECONDS)) {
                throw new AssertionError("sensorFlushFailed");
            }
            result.putInt("sample_count", listener.count);
            result.putString("first_vector", listener.firstVector);
            result.putString("accuracy", "unreliable virtual stationary sample");
            result.putString("ok", "true");
            resultCode = Activity.RESULT_OK;
        } catch (Throwable failure) {
            result.putString("ok", "false");
            result.putString("error", failure.toString());
        } finally {
            if (manager != null) manager.unregisterListener(listener);
            if (callbacks.isAlive()) {
                callbacks.quitSafely();
                try { callbacks.join(1000); } catch (InterruptedException interrupted) {
                    Thread.currentThread().interrupt();
                }
            }
        }
        finish(resultCode, result);
    }

    private static final class Listener implements SensorEventListener2 {
        final CountDownLatch samples = new CountDownLatch(3);
        final CountDownLatch flushed = new CountDownLatch(1);
        volatile String error;
        volatile String firstVector;
        volatile int count;
        private long previousTimestamp;

        @Override public void onSensorChanged(SensorEvent event) {
            if (event.sensor.getType() != Sensor.TYPE_ACCELEROMETER
                    || event.timestamp <= previousTimestamp || event.values.length < 3) {
                error = "invalidSensorEvent";
            } else if (!Float.isFinite(event.values[0]) || !Float.isFinite(event.values[1])
                    || !Float.isFinite(event.values[2])) {
                error = "nonFiniteSensorEvent";
            } else if (Math.abs(event.values[0]) > 0.001f
                    || Math.abs(event.values[1]) > 0.001f
                    || Math.abs(event.values[2] - 9.8f) > 0.001f
                    || event.accuracy != SensorManager.SENSOR_STATUS_UNRELIABLE) {
                error = "unexpectedVirtualStationarySample";
            }
            previousTimestamp = event.timestamp;
            if (count == 0 && event.values.length >= 3) {
                firstVector = event.values[0] + "," + event.values[1] + "," + event.values[2];
            }
            count++;
            samples.countDown();
        }

        @Override public void onAccuracyChanged(Sensor sensor, int accuracy) {}
        @Override public void onFlushCompleted(Sensor sensor) { flushed.countDown(); }
    }
}

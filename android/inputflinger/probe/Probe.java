package com.android.droidloom.inputprobe;

import android.app.Activity;
import android.hardware.Sensor;
import android.hardware.SensorEvent;
import android.hardware.SensorEventListener;
import android.hardware.SensorManager;
import android.media.AudioManager;
import android.media.ToneGenerator;
import android.opengl.GLES20;
import android.opengl.GLSurfaceView;
import android.os.Bundle;
import android.util.Log;
import android.view.InputDevice;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.TextView;
import java.nio.ByteBuffer;
import java.util.Arrays;
import javax.microedition.khronos.egl.EGLConfig;
import javax.microedition.khronos.opengles.GL10;

/** Ordinary app input, graphics and optional output-only audio acceptance. */
public final class Probe extends Activity implements SensorEventListener {
    private static final String TAG = "DroidloomInputProbe";
    private TextView status;
    private SensorManager sensors;
    private GLSurfaceView surface;
    private ToneGenerator tone;
    private boolean sensorLogged;

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        TextView title = new TextView(this);
        title.setText("Droidloom input check");
        title.setTextSize(24);
        root.addView(title);
        EditText text = new EditText(this);
        text.setSingleLine(true);
        text.setHint("Keyboard / clipboard test");
        root.addView(text);
        text.addTextChangedListener(new android.text.TextWatcher() {
            public void beforeTextChanged(CharSequence s, int start, int count, int after) {}
            public void onTextChanged(CharSequence s, int start, int before, int count) {
                Log.i(TAG, "TEXT " + s);
            }
            public void afterTextChanged(android.text.Editable s) {}
        });
        status = new TextView(this);
        status.setText("Pen / mouse target");
        status.setBackgroundColor(0xffe1efff);
        status.setOnTouchListener((view, event) -> { motion(event); return true; });
        status.setOnGenericMotionListener((view, event) -> { motion(event); return true; });
        root.addView(status, new LinearLayout.LayoutParams(-1, 0, 1));
        if (getIntent().getBooleanExtra("graphics", false)) {
            surface = new GLSurfaceView(this);
            surface.setEGLContextClientVersion(2);
            surface.setRenderer(new GraphicsCheck());
            surface.setRenderMode(GLSurfaceView.RENDERMODE_WHEN_DIRTY);
            root.addView(surface, new LinearLayout.LayoutParams(-1, 200));
        }
        setContentView(root);
        text.requestFocus();
        for (int id : InputDevice.getDeviceIds()) {
            InputDevice device = InputDevice.getDevice(id);
            if (device != null) Log.i(TAG, "DEVICE id=" + id + " name=" + device.getName()
                + " type=" + device.getKeyboardType() + " hasKeys=" + Arrays.toString(
                    device.hasKeys(KeyEvent.KEYCODE_SPACE, KeyEvent.KEYCODE_ENTER,
                                   KeyEvent.KEYCODE_A, KeyEvent.KEYCODE_Z)));
        }
        sensors = getSystemService(SensorManager.class);
        Sensor accelerometer = sensors.getDefaultSensor(Sensor.TYPE_ACCELEROMETER);
        Log.i(TAG, "DEFAULT_ACCELEROMETER " + (accelerometer == null ? "missing" : accelerometer.getName()));
        if (accelerometer != null) sensors.registerListener(this, accelerometer, SensorManager.SENSOR_DELAY_NORMAL);
        if (getIntent().getBooleanExtra("audio", false)) {
            tone = new ToneGenerator(AudioManager.STREAM_MUSIC, 20);
            Log.i(TAG, "AUDIO started=" + tone.startTone(ToneGenerator.TONE_PROP_BEEP, 250));
        }
        Log.i(TAG, "READY");
    }

    private void motion(MotionEvent event) {
        String line = "MOTION action=" + event.getActionMasked()
            + " source=" + Integer.toHexString(event.getSource())
            + " tool=" + event.getToolType(0) + " device=" + event.getDeviceId()
            + " meta=" + Integer.toHexString(event.getMetaState())
            + " buttons=" + event.getButtonState() + " pressure=" + event.getPressure()
            + " tilt=" + event.getAxisValue(MotionEvent.AXIS_TILT);
        status.setText(line);
        Log.i(TAG, line);
    }

    @Override public boolean dispatchGenericMotionEvent(MotionEvent event) {
        motion(event);
        return super.dispatchGenericMotionEvent(event);
    }

    @Override public boolean dispatchKeyEvent(KeyEvent event) {
        Log.i(TAG, "KEY action=" + event.getAction() + " code=" + event.getKeyCode()
            + " scan=" + event.getScanCode() + " device=" + event.getDeviceId()
            + " meta=" + Integer.toHexString(event.getMetaState()));
        return super.dispatchKeyEvent(event);
    }

    @Override public void onSensorChanged(SensorEvent event) {
        if (!sensorLogged) {
            sensorLogged = true;
            Log.i(TAG, "ACCELEROMETER accuracy=" + event.accuracy + " values=" + Arrays.toString(event.values));
        }
    }
    @Override public void onAccuracyChanged(Sensor sensor, int accuracy) {}
    @Override public void onPause() {
        if (surface != null) surface.onPause();
        super.onPause();
    }
    @Override public void onResume() {
        super.onResume();
        if (surface != null) surface.onResume();
    }
    @Override public void onDestroy() {
        if (sensors != null) sensors.unregisterListener(this);
        if (tone != null) tone.release();
        super.onDestroy();
    }

    private static final class GraphicsCheck implements GLSurfaceView.Renderer {
        private int program;
        public void onSurfaceCreated(GL10 gl, EGLConfig config) {
            Log.i(TAG, "GL_RENDERER " + GLES20.glGetString(GLES20.GL_RENDERER));
            Log.i(TAG, "GL_VERSION " + GLES20.glGetString(GLES20.GL_VERSION));
            int vertex = compile(GLES20.GL_VERTEX_SHADER,
                "void main(){gl_Position=vec4(0.0,0.0,0.0,1.0);gl_PointSize=32.0;}");
            int fragment = compile(GLES20.GL_FRAGMENT_SHADER,
                "precision mediump float;void main(){gl_FragColor=vec4(1.0,0.0,0.0,1.0);}");
            program = GLES20.glCreateProgram();
            GLES20.glAttachShader(program, vertex);
            GLES20.glAttachShader(program, fragment);
            GLES20.glLinkProgram(program);
            int[] linked = new int[1];
            GLES20.glGetProgramiv(program, GLES20.GL_LINK_STATUS, linked, 0);
            if (linked[0] == 0) throw new AssertionError(GLES20.glGetProgramInfoLog(program));
            GLES20.glDeleteShader(vertex);
            GLES20.glDeleteShader(fragment);
        }
        private static int compile(int kind, String source) {
            int shader = GLES20.glCreateShader(kind);
            GLES20.glShaderSource(shader, source);
            GLES20.glCompileShader(shader);
            int[] compiled = new int[1];
            GLES20.glGetShaderiv(shader, GLES20.GL_COMPILE_STATUS, compiled, 0);
            if (compiled[0] == 0) throw new AssertionError(GLES20.glGetShaderInfoLog(shader));
            return shader;
        }
        public void onSurfaceChanged(GL10 gl, int width, int height) {
            GLES20.glViewport(0, 0, width, height);
        }
        public void onDrawFrame(GL10 gl) {
            GLES20.glClearColor(0, 0, 0, 1);
            GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT);
            GLES20.glUseProgram(program);
            GLES20.glDrawArrays(GLES20.GL_POINTS, 0, 1);
            int[] viewport = new int[4];
            GLES20.glGetIntegerv(GLES20.GL_VIEWPORT, viewport, 0);
            ByteBuffer pixel = ByteBuffer.allocateDirect(4);
            GLES20.glReadPixels(viewport[2] / 2, viewport[3] / 2, 1, 1,
                GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, pixel);
            int error = GLES20.glGetError();
            boolean red = (pixel.get(0) & 255) > 240 && (pixel.get(1) & 255) < 16
                && (pixel.get(2) & 255) < 16 && error == GLES20.GL_NO_ERROR;
            Log.i(TAG, "GL_PIXEL pass=" + red + " error=" + error);
            if (!red) throw new AssertionError("shader draw/readback failed");
        }
    }
}

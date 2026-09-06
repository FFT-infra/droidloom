/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.clipboard;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.OpenableColumns;
import java.io.File;
import java.io.FileNotFoundException;
import java.util.List;

/** Imported content is read-only and accessible only through Android URI grants. */
public final class ClipboardProvider extends ContentProvider {
    static final String AUTHORITY = "com.android.systemui.droidloom.clipboard";
    private static ClipboardBridge.Stored current;
    static synchronized void replace(ClipboardBridge.Stored next) {
        ClipboardBridge.Stored old = current; current = next;
        if (old != null && old != next) old.close();
    }
    static Uri uri(String id, int index) {
        return new Uri.Builder().scheme("content").authority(AUTHORITY)
                .appendPath(id).appendPath(Integer.toString(index)).build();
    }
    private static synchronized Entry entry(Uri uri) throws FileNotFoundException {
        try {
            List<String> parts = uri.getPathSegments();
            if (!AUTHORITY.equals(uri.getAuthority()) || parts.size() != 2 || current == null
                    || !parts.get(0).equals(current.description.getString("id"))) throw new Exception();
            int index = Integer.parseInt(parts.get(1));
            if (index < 0 || index >= current.files.size()) throw new Exception();
            org.json.JSONObject blob = current.description.getJSONArray("blobs").getJSONObject(index);
            return new Entry(current.files.get(index), blob.getString("name"), blob.getString("mime"));
        } catch (Exception e) { throw new FileNotFoundException("Clipboard content expired"); }
    }
    private static final class Entry {
        final File file; final String name; final String mime;
        Entry(File f, String n, String m) { file=f; name=n; mime=m; }
    }
    @Override public boolean onCreate() { return true; }
    @Override public ParcelFileDescriptor openFile(Uri uri, String mode) throws FileNotFoundException {
        if (!"r".equals(mode)) throw new FileNotFoundException("Read-only clipboard");
        synchronized (ClipboardProvider.class) {
            return ParcelFileDescriptor.open(entry(uri).file, ParcelFileDescriptor.MODE_READ_ONLY);
        }
    }
    @Override public String getType(Uri uri) { try {return entry(uri).mime;}catch(FileNotFoundException e){return null;} }
    @Override public Cursor query(Uri uri, String[] projection, String selection, String[] args, String sort) {
        try {
            Entry e=entry(uri);
            if(projection==null) projection=new String[]{OpenableColumns.DISPLAY_NAME,OpenableColumns.SIZE};
            MatrixCursor cursor=new MatrixCursor(projection);Object[] row=new Object[projection.length];
            for(int i=0;i<projection.length;i++){
                if(OpenableColumns.DISPLAY_NAME.equals(projection[i]))row[i]=e.name;
                if(OpenableColumns.SIZE.equals(projection[i]))row[i]=e.file.length();
            }
            cursor.addRow(row);return cursor;
        }catch(FileNotFoundException e){return null;}
    }
    @Override public Uri insert(Uri uri, ContentValues v){throw new UnsupportedOperationException();}
    @Override public int update(Uri uri,ContentValues v,String s,String[] a){throw new UnsupportedOperationException();}
    @Override public int delete(Uri uri,String s,String[] a){throw new UnsupportedOperationException();}
}

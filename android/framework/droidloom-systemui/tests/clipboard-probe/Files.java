package com.android.droidloom.clipboardprobe;
import android.content.*;
import android.net.Uri;
import android.database.*;
import android.os.ParcelFileDescriptor;
import java.io.*;
public final class Files extends ContentProvider {
    @Override public boolean onCreate(){return true;}
    private File file(Uri uri)throws FileNotFoundException{String n=uri.getLastPathSegment();if(n==null||n.contains("/")||n.equals(".."))throw new FileNotFoundException();return new File(getContext().getFilesDir(),n);}
    @Override public ParcelFileDescriptor openFile(Uri uri,String mode)throws FileNotFoundException{if(!mode.equals("r"))throw new FileNotFoundException();return ParcelFileDescriptor.open(file(uri),ParcelFileDescriptor.MODE_READ_ONLY);}
    @Override public String getType(Uri uri){return uri.getQueryParameter("mime");}
    @Override public Cursor query(Uri uri,String[] p,String s,String[] a,String o){MatrixCursor c=new MatrixCursor(new String[]{"_display_name","_size"});try{File f=file(uri);c.addRow(new Object[]{f.getName(),f.length()});return c;}catch(Exception e){return null;}}
    @Override public Uri insert(Uri u,ContentValues v){throw new UnsupportedOperationException();}
    @Override public int update(Uri u,ContentValues v,String s,String[] a){throw new UnsupportedOperationException();}
    @Override public int delete(Uri u,String s,String[] a){throw new UnsupportedOperationException();}
}

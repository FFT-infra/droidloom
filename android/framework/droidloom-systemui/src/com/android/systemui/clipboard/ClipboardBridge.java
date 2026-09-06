/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.clipboard;

import android.content.ClipData;
import android.content.ClipDescription;
import android.content.ClipboardManager;
import android.content.Context;
import android.database.Cursor;
import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.net.Uri;
import android.os.Handler;
import android.os.Looper;
import android.os.PersistableBundle;
import android.provider.OpenableColumns;
import org.json.JSONArray;
import org.json.JSONObject;
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.ArrayDeque;
import java.util.UUID;

/** Event-driven clipboard adapter; all content I/O runs away from main/Binder threads. */
public final class ClipboardBridge implements AutoCloseable {
    private static final int ABI=1, MAX_TEXT=1024*1024, MAX_FRAME=3*MAX_TEXT+65536;
    private static final long MAX_BYTES=256L*1024*1024;
    private static final String ORIGIN="com.android.droidloom.clipboard.origin";
    private final Context context;
    private final ClipboardManager clipboard;
    private final Handler main=new Handler(Looper.getMainLooper());
    private final Object monitor=new Object();
    private final ArrayDeque<JSONObject> controls=new ArrayDeque<>();
    private final String session=UUID.randomUUID().toString().replace("-", "");
    private final ClipboardManager.OnPrimaryClipChangedListener listener=this::changed;
    private final File directory;
    private volatile LocalSocket socket;
    private volatile boolean stopped;
    private boolean changed;
    private volatile long base;
    private long sequence;
    private volatile long localChanges;
    private volatile String imported;
    private String lastObserved;
    private volatile long imports,exports;

    public ClipboardBridge(Context context){
        this.context=context;
        clipboard=context.getSystemService(ClipboardManager.class);
        directory=new File(context.getCacheDir(), "droidloom-clipboard");
    }
    public void start(){
        if(!new File("/dev/socket/droidloom/clipboard").exists())return;
        directory.mkdirs();
        File[] old=directory.listFiles();if(old!=null)for(File f:old)if(f.isFile())f.delete();
        clipboard.addPrimaryClipChangedListener(listener);
        new Thread(this::connect,"droidloom-clipboard").start();
    }
    private void changed(){
        ClipDescription description=clipboard.getPrimaryClipDescription();
        if(description==null && "clear".equals(imported))return;
        if(description!=null && description.getExtras()!=null
                && imported!=null && imported.equals(description.getExtras().getString(ORIGIN)))return;
        localChanges++;
        synchronized(monitor){changed=true;monitor.notifyAll();}
    }
    public void dump(PrintWriter out){
        out.println("DROIDLOOM_CLIPBOARD_ABI=1;");
        out.println("clipboardConnected="+(socket!=null)+" imports="+imports+" exports="+exports);
    }
    @Override public void close(){stopped=true;clipboard.removePrimaryClipChangedListener(listener);close(socket);synchronized(monitor){monitor.notifyAll();}}
    private static void close(LocalSocket s){
        if(s==null)return;
        try{s.shutdownInput();}catch(Exception ignored){}
        try{s.shutdownOutput();}catch(Exception ignored){}
        try{s.close();}catch(Exception ignored){}
    }
    private void connect(){
        while(!stopped){
            try(LocalSocket client=new LocalSocket()){
                client.connect(new LocalSocketAddress("droidloom-clipboard"));
                if(client.getPeerCredentials().getUid()!=0)throw new IOException("Untrusted clipboard relay");
                DataInputStream in=new DataInputStream(client.getInputStream());
                DataOutputStream out=new DataOutputStream(client.getOutputStream());
                message(out,new JSONObject().put("type","hello").put("abi",ABI));
                JSONObject hello=json(readFrame(in));
                if(!"hello".equals(hello.getString("type"))||hello.getInt("abi")!=ABI)throw new IOException("Clipboard ABI mismatch");
                synchronized(monitor){controls.clear();changed=false;socket=client;}
                Thread writer=new Thread(()->writeLoop(client,out),"droidloom-clipboard-writer");writer.start();
                try{readLoop(client,in);}finally{close(client);synchronized(monitor){if(socket==client)socket=null;monitor.notifyAll();}writer.join(5000);}
            }catch(Exception error){android.util.Log.w("DroidloomClipboard","Transport disconnected: "+error.getClass().getSimpleName());}
            finally{socket=null;}
            if(!stopped)try{Thread.sleep(1000);}catch(InterruptedException e){return;}
        }
    }
    private void writeLoop(LocalSocket client,DataOutputStream out){
        try{
            while(!stopped){
                JSONObject control=null;boolean snapshot=false;
                synchronized(monitor){
                    while(!stopped&&socket==client&&controls.isEmpty()&&!changed)monitor.wait();
                    if(stopped||socket!=client)return;
                    if(!controls.isEmpty())control=controls.removeFirst();else{snapshot=changed;changed=false;}
                }
                if(control!=null){message(out,control);continue;}
                if(snapshot){
                    Stored snapshotData;
                    try{snapshotData=snapshot();}
                    catch(Exception error){lastObserved=null;android.util.Log.w("DroidloomClipboard","Content export unavailable: "+error.getClass().getSimpleName());continue;}
                    try(Stored data=snapshotData){
                        if(data==null)continue;
                        message(out,new JSONObject().put("type","begin").put("clip",data.description));
                        byte[] buffer=new byte[65536];
                        for(int i=0;i<data.files.size();i++){
                            try(InputStream file=new FileInputStream(data.files.get(i))){int n;while((n=file.read(buffer))>=0){
                                out.writeInt(n+5);out.writeByte(2);out.writeInt(i);out.write(buffer,0,n);
                            }}
                        }
                        message(out,new JSONObject().put("type","end").put("id",data.description.getString("id")));
                        exports++;
                    }
                }
            }
        }catch(Exception error){android.util.Log.w("DroidloomClipboard","Export failed: "+error.getClass().getSimpleName());close(client);}
    }
    private Stored snapshot() throws Exception {
        long expectedChanges=localChanges;
        ClipData clip=clipboard.getPrimaryClip();
        if(clip!=null&&clip.getDescription().getExtras()!=null){
            String origin=clip.getDescription().getExtras().getString(ORIGIN);
            if(origin!=null&&origin.equals(imported))return null;
        }
        if(clip==null&&"clear".equals(imported))return null;
        StringBuilder identity=new StringBuilder();
        if(clip!=null)for(int i=0;i<clip.getItemCount();i++){
            ClipData.Item item=clip.getItemAt(i);identity.append(item.getText()).append('\0').append(item.getHtmlText()).append('\0').append(item.getUri()).append('\0');
        }
        String observed=identity.toString();
        if(observed.equals(lastObserved))return null;
        lastObserved=observed;imported=null;
        boolean referencesImported=false;
        if(clip!=null)for(int i=0;i<clip.getItemCount();i++){
            Uri uri=clip.getItemAt(i).getUri();
            if(uri!=null && ClipboardProvider.AUTHORITY.equals(uri.getAuthority()))referencesImported=true;
        }
        if(!referencesImported)ClipboardProvider.replace(null);
        JSONObject desc=new JSONObject().put("id",session+"-"+(++sequence)).put("base",base)
                .put("text",JSONObject.NULL).put("html",JSONObject.NULL).put("blobs",new JSONArray());
        Stored result=new Stored(desc);long total=0;
        try{
            if(clip!=null){
                StringBuilder text=new StringBuilder();String html=null;
                if(clip.getItemCount()>64)throw new IOException("Too many clipboard items");
                for(int i=0;i<clip.getItemCount();i++){
                    ClipData.Item item=clip.getItemAt(i);
                    if(item.getText()!=null){if(text.length()>0)text.append('\n');text.append(item.getText());}
                    if(i==0)html=item.getHtmlText();
                    Uri uri=item.getUri();if(uri==null)continue;
                    if(!"content".equals(uri.getScheme())&&!"file".equals(uri.getScheme())){
                        if(text.length()>0)text.append('\n');text.append(uri);continue;
                    }
                    String mime=context.getContentResolver().getType(uri);if(mime==null)mime="application/octet-stream";
                    String name="clipboard.bin";
                    try(Cursor cursor=context.getContentResolver().query(uri,new String[]{OpenableColumns.DISPLAY_NAME},null,null,null)){
                        if(cursor!=null&&cursor.moveToFirst()&&!cursor.isNull(0))name=cursor.getString(0);
                    }catch(Exception ignored){}
                    name=safeName(name);
                    File target=File.createTempFile("export-",".bin",directory);result.files.add(target);
                    long count=0;
                    try(android.content.res.AssetFileDescriptor asset=context.getContentResolver().openAssetFileDescriptor(uri,"r")){
                        if(asset==null)throw new IOException("Unreadable content");
                        try(InputStream input=asset.createInputStream();OutputStream output=new FileOutputStream(target)){
                            byte[] bytes=new byte[65536];int n;
                            long deadline=android.os.SystemClock.elapsedRealtime()+30000;
                            android.system.StructPollfd poll=new android.system.StructPollfd();
                            poll.fd=asset.getFileDescriptor();poll.events=(short)android.system.OsConstants.POLLIN;
                            while(true){
                                if(stopped||localChanges!=expectedChanges||android.os.SystemClock.elapsedRealtime()>deadline)throw new IOException("Clipboard transfer cancelled");
                                if(android.system.Os.poll(new android.system.StructPollfd[]{poll},100)==0)continue;
                                n=input.read(bytes);if(n<0)break;count+=n;
                                if(total+count>MAX_BYTES)throw new IOException("Clipboard limit");output.write(bytes,0,n);
                            }
                        }
                    }
                    total+=count;
                    desc.getJSONArray("blobs").put(new JSONObject().put("name",name).put("mime",mime).put("size",count));
                }
                if(text.length()>0||clip.getItemCount()>0&&clip.getItemAt(0).getText()!=null)desc.put("text",text.toString());
                if(html!=null)desc.put("html",html);
            }
            validate(desc);return result;
        }catch(Exception error){result.close();throw error;}
    }
    private void readLoop(LocalSocket client,DataInputStream in)throws Exception{
        Stored pending=null;long changedAtBegin=0;
        try{
            while(!stopped&&socket==client){
                byte[] frame=readFrame(in);
                if(frame[0]==2){
                    if(pending==null||frame.length<5||frame.length>65541)throw new IOException("Unexpected clipboard data");
                    int index=java.nio.ByteBuffer.wrap(frame,1,4).getInt();
                    if(index<0||index>=pending.files.size())throw new IOException("Invalid clipboard item");
                    File file=pending.files.get(index);long maximum=pending.description.getJSONArray("blobs").getJSONObject(index).getLong("size");
                    if(file.length()+frame.length-5>maximum)throw new IOException("Clipboard item overflow");
                    try(OutputStream output=new FileOutputStream(file,true)){output.write(frame,5,frame.length-5);}continue;
                }
                JSONObject message=json(frame);String type=message.getString("type");
                if("sync".equals(type)){base=message.getLong("revision");}
                else if("begin".equals(type)){
                    if(pending!=null)pending.close();
                    JSONObject desc=message.getJSONObject("clip");validate(desc);pending=new Stored(desc);
                    base=desc.getLong("base");changedAtBegin=localChanges;
                    JSONArray blobs=desc.getJSONArray("blobs");
                    for(int i=0;i<blobs.length();i++)pending.files.add(File.createTempFile("import-",".bin",directory));
                }else if("end".equals(type)){
                    if(pending==null||!pending.description.getString("id").equals(message.getString("id")))throw new IOException("Clipboard transaction mismatch");
                    JSONArray blobs=pending.description.getJSONArray("blobs");
                    for(int i=0;i<blobs.length();i++)if(pending.files.get(i).length()!=blobs.getJSONObject(i).getLong("size"))throw new IOException("Incomplete clipboard item");
                    Stored ready=pending;pending=null;long expectedChanges=changedAtBegin;
                    main.post(()->apply(client,ready,expectedChanges));
                }else throw new IOException("Unexpected clipboard command");
            }
        }finally{if(pending!=null)pending.close();}
    }
    private void apply(LocalSocket client,Stored data,long expectedChanges){
        try{
            String id=data.description.getString("id");
            // A new local copy while bytes were arriving takes precedence over that transfer.
            if(socket!=client||localChanges!=expectedChanges){data.close();ack(id);return;}
            String text=data.description.isNull("text")?null:data.description.getString("text");
            String html=data.description.isNull("html")?null:data.description.getString("html");
            if(html!=null && text==null)text=android.text.Html.fromHtml(html,android.text.Html.FROM_HTML_MODE_LEGACY).toString();
            JSONArray blobs=data.description.getJSONArray("blobs");
            if(text==null&&html==null&&blobs.length()==0){imported="clear";lastObserved=null;clipboard.clearPrimaryClip();ClipboardProvider.replace(null);data.close();}
            else{
                ArrayList<String> mimes=new ArrayList<>();if(text!=null)mimes.add("text/plain");if(html!=null)mimes.add("text/html");
                for(int i=0;i<blobs.length();i++){String mime=blobs.getJSONObject(i).getString("mime");if(!mimes.contains(mime))mimes.add(mime);}
                ClipDescription description=new ClipDescription("Desktop clipboard",mimes.toArray(new String[0]));
                PersistableBundle extras=new PersistableBundle();extras.putString(ORIGIN,id);description.setExtras(extras);
                Uri first=blobs.length()>0?ClipboardProvider.uri(id,0):null;
                ClipData.Item item=new ClipData.Item(text,html,null,first);
                ClipData clip=new ClipData(description,item);
                for(int i=1;i<blobs.length();i++)clip.addItem(new ClipData.Item(ClipboardProvider.uri(id,i)));
                ClipboardProvider.replace(data);imported=id;lastObserved=null;clipboard.setPrimaryClip(clip);
            }
            imports++;ack(id);
        }catch(Exception error){data.close();android.util.Log.w("DroidloomClipboard","Import failed: "+error.getClass().getSimpleName());close(client);}
    }
    private void ack(String id)throws Exception{
        synchronized(monitor){if(controls.size()>=32)throw new IOException("Clipboard control overflow");controls.add(new JSONObject().put("type","ack").put("id",id));monitor.notifyAll();}
    }
    static final class Stored implements AutoCloseable {
        final JSONObject description;final ArrayList<File> files=new ArrayList<>();
        Stored(JSONObject description){this.description=description;}
        @Override public void close(){for(File file:files)file.delete();}
    }
    private static byte[] readFrame(DataInputStream input)throws IOException{
        int n=input.readInt();if(n<1||n>MAX_FRAME)throw new IOException("Clipboard frame limit");byte[] bytes=new byte[n];input.readFully(bytes);return bytes;
    }
    private static JSONObject json(byte[] frame)throws Exception{
        if(frame[0]!=1)throw new IOException("Expected clipboard metadata");return new JSONObject(new String(frame,1,frame.length-1,StandardCharsets.UTF_8));
    }
    private static void message(DataOutputStream output,JSONObject message)throws Exception{
        byte[] bytes=message.toString().getBytes(StandardCharsets.UTF_8);if(bytes.length+1>MAX_FRAME)throw new IOException("Clipboard frame limit");
        output.writeInt(bytes.length+1);output.writeByte(1);output.write(bytes);
    }
    private static void validate(JSONObject desc)throws Exception{
        String id=desc.getString("id");if(!id.matches("[A-Za-z0-9-]{1,96}")||desc.getLong("base")<0)throw new IOException("Clipboard identity");
        for(String key:new String[]{"text","html"})if(!desc.isNull(key)&&desc.getString(key).getBytes(StandardCharsets.UTF_8).length>MAX_TEXT)throw new IOException("Clipboard text limit");
        JSONArray blobs=desc.getJSONArray("blobs");if(blobs.length()>64)throw new IOException("Clipboard file count");long total=0;
        for(int i=0;i<blobs.length();i++){
            JSONObject b=blobs.getJSONObject(i);long size=b.getLong("size");if(size<0||size>MAX_BYTES-total)throw new IOException("Clipboard size limit");total+=size;
            String name=b.getString("name"),mime=b.getString("mime");
            if(name.getBytes(StandardCharsets.UTF_8).length>255||mime.length()>128||!mime.contains("/")||mime.chars().anyMatch(Character::isISOControl))throw new IOException("Clipboard metadata limit");
        }
    }
    private static String safeName(String name){String clean=name.replaceAll("[\\p{Cntrl}/\\\\]","");if(clean.isEmpty())return "clipboard.bin";while(clean.getBytes(StandardCharsets.UTF_8).length>255)clean=clean.substring(0,clean.length()-1);return clean;}
}

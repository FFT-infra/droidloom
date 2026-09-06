package com.android.droidloom.clipboardprobe;
import android.content.*;
import android.net.Uri;
import android.database.Cursor;
import android.provider.OpenableColumns;
import org.json.*;
import java.io.*;
import java.security.MessageDigest;
public final class Probe extends BroadcastReceiver {
    static String hash(InputStream in)throws Exception{MessageDigest digest=MessageDigest.getInstance("SHA-256");byte[] buf=new byte[65536];int n;while((n=in.read(buf))>=0)digest.update(buf,0,n);return java.util.HexFormat.of().formatHex(digest.digest());}
    static String hash(String text)throws Exception{return hash(new ByteArrayInputStream(text.getBytes(java.nio.charset.StandardCharsets.UTF_8)));}
    @Override public void onReceive(Context context,Intent intent){PendingResult result=goAsync();new Thread(()->{
        try{
            ClipboardManager cb=context.getSystemService(ClipboardManager.class);String command=intent.getStringExtra("command");JSONObject reply=new JSONObject();
            if("text".equals(command))cb.setPrimaryClip(ClipData.newPlainText("Clipboard test",intent.getStringExtra("text")));
            else if("html".equals(command))cb.setPrimaryClip(ClipData.newHtmlText("Clipboard test",intent.getStringExtra("text"),intent.getStringExtra("html")));
            else if("file".equals(command)){
                String paths=intent.getStringExtra("paths");String mime=intent.getStringExtra("mime");if(mime==null)mime="application/octet-stream";
                ClipData data=null;int index=0;
                for(String path:paths.split("\n")){
                    File source=new File(path);File dest=new File(context.getFilesDir(),source.getName());
                    try(InputStream in=new FileInputStream(source);OutputStream out=new FileOutputStream(dest)){in.transferTo(out);}
                    Uri uri=new Uri.Builder().scheme("content").authority("com.android.droidloom.clipboardprobe").appendPath(source.getName()).appendQueryParameter("mime",mime).build();
                    if(data==null)data=new ClipData("Clipboard files",new String[]{mime},new ClipData.Item(uri));else data.addItem(new ClipData.Item(uri));index++;
                }cb.setPrimaryClip(data);
            }else if("clear".equals(command))cb.clearPrimaryClip();
            else if("get".equals(command)){
                ClipData data=cb.getPrimaryClip();reply.put("empty",data==null);
                if(data!=null){JSONArray items=new JSONArray();for(int i=0;i<data.getItemCount();i++){
                    ClipData.Item item=data.getItemAt(i);JSONObject row=new JSONObject();
                    if(item.getText()!=null)row.put("text_sha256",hash(item.getText().toString()));
                    if(item.getHtmlText()!=null)row.put("html_sha256",hash(item.getHtmlText()));
                    if(item.getUri()!=null){Uri uri=item.getUri();row.put("mime",context.getContentResolver().getType(uri));
                        try(InputStream in=context.getContentResolver().openInputStream(uri)){row.put("bytes_sha256",hash(in));}
                        try(Cursor cursor=context.getContentResolver().query(uri,null,null,null,null)){if(cursor!=null&&cursor.moveToFirst()){int name=cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);if(name>=0)row.put("name",cursor.getString(name));}}
                    }items.put(row);
                }reply.put("items",items);}
            }else throw new IllegalArgumentException("Unknown clipboard test command");
            reply.put("ok",true);result.setResultCode(0);result.setResultData(reply.toString());
        }catch(Exception e){result.setResultCode(1);result.setResultData("{\"ok\":false,\"error\":\""+e.getClass().getSimpleName()+"\"}");}finally{result.finish();}
    },"clipboard-probe").start();}
}

/* clang-format off */
#include "jni.h"
#include <assert.h>
#include <stdint.h>
#include <stdlib.h>
#include "cubeb-jni-instances.h"
/* clang-format on */

#define AUDIO_STREAM_TYPE_MUSIC 3

struct cubeb_jni {
  jobject s_audio_manager_obj = nullptr;
  jclass s_audio_manager_class = nullptr;
  jmethodID s_get_output_latency_id = nullptr;
  jmethodID s_get_property_id = nullptr;
};

extern "C" cubeb_jni *
cubeb_jni_init()
{
  jobject ctx_obj = cubeb_jni_get_context_instance();
  JNIEnv * jni_env = cubeb_get_jni_env_for_thread();
  if (!jni_env || !ctx_obj) {
    return nullptr;
  }

  cubeb_jni * cubeb_jni_ptr = new cubeb_jni;
  assert(cubeb_jni_ptr);

  // Find the audio manager object and make it global to call it from another
  // method
  jclass context_class = jni_env->FindClass("android/content/Context");
  jfieldID audio_service_field = jni_env->GetStaticFieldID(
      context_class, "AUDIO_SERVICE", "Ljava/lang/String;");
  jstring jstr = (jstring)jni_env->GetStaticObjectField(context_class,
                                                        audio_service_field);
  jmethodID get_system_service_id =
      jni_env->GetMethodID(context_class, "getSystemService",
                           "(Ljava/lang/String;)Ljava/lang/Object;");
  jobject audio_manager_obj =
      jni_env->CallObjectMethod(ctx_obj, get_system_service_id, jstr);
  cubeb_jni_ptr->s_audio_manager_obj =
      reinterpret_cast<jobject>(jni_env->NewGlobalRef(audio_manager_obj));

  // Make the audio manager class a global reference in order to preserve method
  // id
  jclass audio_manager_class = jni_env->FindClass("android/media/AudioManager");
  cubeb_jni_ptr->s_audio_manager_class =
      reinterpret_cast<jclass>(jni_env->NewGlobalRef(audio_manager_class));
  cubeb_jni_ptr->s_get_output_latency_id =
      jni_env->GetMethodID(audio_manager_class, "getOutputLatency", "(I)I");
  cubeb_jni_ptr->s_get_property_id = jni_env->GetMethodID(
      audio_manager_class, "getProperty",
      "(Ljava/lang/String;)Ljava/lang/String;");

  jni_env->DeleteLocalRef(ctx_obj);
  jni_env->DeleteLocalRef(context_class);
  jni_env->DeleteLocalRef(jstr);
  jni_env->DeleteLocalRef(audio_manager_obj);
  jni_env->DeleteLocalRef(audio_manager_class);

  return cubeb_jni_ptr;
}

extern "C" int
cubeb_get_output_latency_from_jni(cubeb_jni * cubeb_jni_ptr)
{
  assert(cubeb_jni_ptr);
  JNIEnv * jni_env = cubeb_get_jni_env_for_thread();
  return jni_env->CallIntMethod(
      cubeb_jni_ptr->s_audio_manager_obj,
      cubeb_jni_ptr->s_get_output_latency_id,
      AUDIO_STREAM_TYPE_MUSIC); // param: AudioManager.STREAM_MUSIC
}

// Returns the integer value of an AudioManager property, or -1 on failure.
static int
cubeb_get_int_property_from_jni(cubeb_jni * cubeb_jni_ptr, char const * name)
{
  assert(cubeb_jni_ptr);
  JNIEnv * jni_env = cubeb_get_jni_env_for_thread();
  if (!jni_env || !cubeb_jni_ptr->s_get_property_id) {
    return -1;
  }

  jstring jname = jni_env->NewStringUTF(name);
  jstring jvalue = (jstring)jni_env->CallObjectMethod(
      cubeb_jni_ptr->s_audio_manager_obj, cubeb_jni_ptr->s_get_property_id,
      jname);
  jni_env->DeleteLocalRef(jname);
  if (jni_env->ExceptionCheck()) {
    jni_env->ExceptionClear();
    return -1;
  }
  if (!jvalue) {
    return -1;
  }

  int result = -1;
  char const * value = jni_env->GetStringUTFChars(jvalue, nullptr);
  if (value) {
    char * end = nullptr;
    long parsed = strtol(value, &end, 10);
    if (end != value && *end == '\0' && parsed > 0 && parsed <= INT32_MAX) {
      result = static_cast<int>(parsed);
    }
    jni_env->ReleaseStringUTFChars(jvalue, value);
  }
  jni_env->DeleteLocalRef(jvalue);
  return result;
}

extern "C" int
cubeb_get_output_sample_rate_from_jni(cubeb_jni * cubeb_jni_ptr)
{
  return cubeb_get_int_property_from_jni(
      cubeb_jni_ptr, "android.media.property.OUTPUT_SAMPLE_RATE");
}

extern "C" int
cubeb_get_output_frames_per_buffer_from_jni(cubeb_jni * cubeb_jni_ptr)
{
  return cubeb_get_int_property_from_jni(
      cubeb_jni_ptr, "android.media.property.OUTPUT_FRAMES_PER_BUFFER");
}

extern "C" void
cubeb_jni_destroy(cubeb_jni * cubeb_jni_ptr)
{
  assert(cubeb_jni_ptr);

  JNIEnv * jni_env = cubeb_get_jni_env_for_thread();
  assert(jni_env);

  jni_env->DeleteGlobalRef(cubeb_jni_ptr->s_audio_manager_obj);
  jni_env->DeleteGlobalRef(cubeb_jni_ptr->s_audio_manager_class);

  delete cubeb_jni_ptr;
}
